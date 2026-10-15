// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The address request-budget governor (plan §6.3).
//!
//! Hyperliquid meters L1 actions **per address**, not per connection
//! or per key:
//!
//! ```text
//!   allowance = 10_000 + (1 request per USDC of lifetime volume)
//! ```
//!
//! and an address that spends its allowance drops to **one request
//! every ten seconds**. That is not a rate limit in the usual sense —
//! it is a cliff, it is per-address so a reconnect does not clear it,
//! and at one request per ten seconds this engine cannot place, cancel
//! or reconcile at anything like the rate the strategy needs. An
//! exhausted address is a bricked address until volume accrues.
//!
//! The governor's whole job is to stop before the cliff rather than
//! discover it.
//!
//! ## Two asymmetries, both deliberate
//!
//! **Cancels are exempt.** Hyperliquid grants cancels a separate,
//! larger allowance, and more to the point: a halted engine must
//! always be able to flatten. A governor that refused a cancel because
//! the budget was low would strand exactly the position it was
//! protecting.
//!
//! **Volume is counted from VENUE FILLS ONLY** — never from what the
//! engine believes it traded. The whole point of the number is to
//! predict the venue's own accounting, and the engine's beliefs are
//! precisely what reconciliation exists to doubt.
//!
//! ## Where the figures come from (E7-F1, 2026-09-19)
//!
//! **The venue states them.** `/info {"type":"userRateLimit"}` answers
//! `{"cumVlm":"<USDC>","nRequestsUsed":N,"nRequestsCap":10000+cumVlm,
//! …}` for the master address — the two integers this governor models,
//! from the accounting it exists to predict. The arm reads them once at
//! boot ([`rate_limit_request`] / [`scan_rate_limit`]) and starts from
//! [`AddressBudget::from_venue`].
//!
//! The first cut believed the opposite (⛏ §0.1-8: `userFees.dailyUserVlm`
//! is daily, "the venue exposes no lifetime figure") and so started
//! every unknown address COLD — `spent = initial_buffer`, remaining 0 —
//! "until the engine has watched itself trade". A fresh address can
//! never earn that way, because a remaining of 0 is under any floor
//! and the floor refuses the very submits that would earn it: the first
//! mainnet boot (2026-09-19 13:00:40Z) halted `budget-floor` before its
//! first order. The cold budget remains the FALLBACK for a venue that
//! does not answer at boot, and the state file remains the persistence
//! between reads — it is two integers and the address they belong to.

/// Hyperliquid's starting allowance for a fresh address.
pub const INITIAL_BUFFER: u64 = 10_000;

/// USDC (1e6) of volume that earns one request.
const USDC_PER_REQUEST_1E6: i64 = 1_000_000;

/// XH4e — what the venue's separate CANCEL allowance adds to the request
/// cap: min(cap + 100 000, 2 × cap) ([`AddressBudget::cancel_remaining`]).
const CANCEL_EXTRA: i64 = 100_000;

/// Why a submit was refused.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BudgetErr {
    /// Headroom is at or below the configured floor.
    Floor,
}

/// The per-address request budget.
///
/// Single-writer by construction: it lives on the dispatcher worker
/// thread, which is the only thread that sends an L1 action or reads a
/// venue fill. No atomics on the mutating path — the gauge mirror
/// below is the only thing another thread reads, and it is written
/// once per cold block.
#[repr(C, align(64))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AddressBudget {
    /// Every L1 action this address has sent, across boots.
    spent_requests: u64,
    /// Venue-observed traded notional, USDC 1e6. **Fills only.**
    traded_usdc_1e6: i64,
    /// The GRANT: the venue's starting allowance plus every request
    /// weight this address has reserved and paid for (S7-L1 —
    /// `nRequestsSurplus` at boot, [`AddressBudget::on_reserved`]
    /// after).
    initial_buffer: u64,
    /// Refuse a submit when headroom drops to this.
    floor: u64,
    /// The address these numbers describe. A state file for a
    /// different address is not this address's history.
    address: [u8; 20],
    _pad: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<AddressBudget>() == 64);

impl AddressBudget {
    /// A cold-boot budget: assume the allowance is already spent.
    #[must_use]
    pub fn cold(address: [u8; 20], floor: u64) -> Self {
        Self {
            spent_requests: INITIAL_BUFFER,
            traded_usdc_1e6: 0,
            initial_buffer: INITIAL_BUFFER,
            floor,
            address,
            _pad: [0; 12],
        }
    }

    /// The budget as the VENUE states it: `nRequestsUsed` spent,
    /// `cumVlm` traded, `nRequestsSurplus` reserved beyond use.
    /// `remaining()` is then exactly the venue's
    /// `nRequestsCap − nRequestsUsed + nRequestsSurplus`.
    ///
    /// **S7-L1 — why the surplus is part of the grant.** The venue nets
    /// reserved weight against use: `nRequestsUsed` is
    /// `max(0, used − reserved)` and `nRequestsSurplus` is
    /// `max(0, reserved − used)`, so the headroom is
    /// `cap − used + reserved` either way. Dropping the surplus would
    /// read an address that had bought headroom as one that had not.
    #[must_use]
    pub fn from_venue(
        address: [u8; 20],
        floor: u64,
        used: u64,
        cum_vlm_1e6: i64,
        surplus: u64,
    ) -> Self {
        let mut b = Self::restored(address, floor, used, cum_vlm_1e6);
        b.initial_buffer = INITIAL_BUFFER.saturating_add(surplus);
        b
    }

    /// **S7-L1** — the venue accepted a `reserveRequestWeight` of
    /// `weight`: this address may send that many more requests. Added
    /// to the grant, which is where the venue's own arithmetic puts it
    /// (see [`Self::from_venue`]).
    #[inline]
    pub fn on_reserved(&mut self, weight: u64) {
        self.initial_buffer = self.initial_buffer.saturating_add(weight);
    }

    /// A budget restored from known figures.
    ///
    /// The grant restores to the venue's starting allowance: weight
    /// reserved since is not in the state file, so a boot that falls
    /// back to the file undercounts its headroom — the conservative
    /// direction — and the venue read at boot restores it.
    #[must_use]
    pub fn restored(
        address: [u8; 20],
        floor: u64,
        spent_requests: u64,
        traded_usdc_1e6: i64,
    ) -> Self {
        Self {
            spent_requests,
            traded_usdc_1e6,
            initial_buffer: INITIAL_BUFFER,
            floor,
            address,
            _pad: [0; 12],
        }
    }

    /// **XMM XH5c-2** — [`Self::remaining`] less what traded volume earned
    /// (a request a USDC): the grant (the starting allowance and the weight
    /// reserved) less the requests spent — what a purchase moves and volume
    /// does not. The gateway arm judges a top-up on it, so a minute's
    /// volume is never read as a purchase.
    #[inline]
    #[must_use]
    pub fn remaining_unearned(&self) -> i64 {
        let spent = i64::try_from(self.spent_requests).unwrap_or(i64::MAX);
        i64::try_from(self.initial_buffer)
            .unwrap_or(i64::MAX)
            .saturating_sub(spent)
    }

    /// Requests believed to remain before the venue's cliff.
    ///
    /// Signed, and allowed to go negative: a governor that saturated
    /// at zero would report the same number for "just spent" and "a
    /// thousand over", and the difference is how far behind the
    /// engine's model of the venue has fallen.
    #[inline(always)]
    #[must_use]
    pub fn remaining(&self) -> i64 {
        // `spent_requests as i64` would WRAP: any value above
        // i64::MAX casts negative and turns this subtraction into an
        // addition, so a maximally-spent address would report
        // near-full headroom — the exact permissive failure this type
        // exists to prevent. Clamp instead.
        let spent = i64::try_from(self.spent_requests).unwrap_or(i64::MAX);
        (self.initial_buffer as i64)
            .saturating_add(self.traded_usdc_1e6 / USDC_PER_REQUEST_1E6)
            .saturating_sub(spent)
    }

    /// May a SUBMIT be sent?
    ///
    /// Checked before signing, not after: a signed action that is then
    /// discarded has still burned a nonce.
    #[inline(always)]
    pub fn may_submit(&self) -> Result<(), BudgetErr> {
        let floor = i64::try_from(self.floor).unwrap_or(i64::MAX);
        if self.remaining() <= floor {
            return Err(BudgetErr::Floor);
        }
        Ok(())
    }

    /// **XH4e — requests the venue's separate cancel allowance leaves**:
    /// min(cap + 100 000, 2 × cap) − spent, the cap being the allowance
    /// [`Self::remaining`] counts against. Reported, never used to refuse:
    /// a cancel always goes (below). The venue's netting of reserved
    /// weight into this allowance is unmeasured; the grant carries it.
    #[must_use]
    pub fn cancel_remaining(&self) -> i64 {
        let cap = i64::try_from(self.initial_buffer)
            .unwrap_or(i64::MAX)
            .saturating_add(self.traded_usdc_1e6 / USDC_PER_REQUEST_1E6);
        let spent = i64::try_from(self.spent_requests).unwrap_or(i64::MAX);
        cap.saturating_add(CANCEL_EXTRA)
            .min(cap.saturating_mul(2))
            .saturating_sub(spent)
    }

    /// Cancels are always permitted. See the module docs: a halted
    /// engine must be able to flatten.
    #[inline(always)]
    #[must_use]
    pub const fn may_cancel(&self) -> bool {
        true
    }

    /// One L1 action left the host carrying `items` orders. Call for
    /// EVERY action, cancels included — the venue counts them even
    /// where it grants them a separate allowance, and a governor that
    /// undercounted would drift optimistic, which is the wrong
    /// direction. **A batch of `n` is `n` address requests** (§2.2):
    /// the first cut charged one per POST, which was right only
    /// because every caller happened to send one-element batches.
    #[inline(always)]
    pub fn on_action_sent(&mut self, items: u32) {
        self.spent_requests = self.spent_requests.saturating_add(u64::from(items.max(1)));
    }

    /// **XH4e — `items` requests counted by [`Self::on_action_sent`] never
    /// left the host** (the gateway refused them unsent: throttled, halted,
    /// not ready): taken back, so a refused burst cannot walk the count to
    /// the floor. Saturating, like the charge.
    #[inline(always)]
    pub fn on_action_unsent(&mut self, items: u32) {
        self.spent_requests = self.spent_requests.saturating_sub(u64::from(items));
    }

    /// A venue fill was observed. `notional_usdc_1e6` must come from
    /// the fill, never from the order.
    #[inline(always)]
    pub fn on_venue_fill(&mut self, notional_usdc_1e6: i64) {
        // abs: a sell is volume too. `saturating_abs` because
        // i64::MIN has no positive counterpart and a hostile figure
        // must not panic on the fill path.
        self.traded_usdc_1e6 = self
            .traded_usdc_1e6
            .saturating_add(notional_usdc_1e6.saturating_abs());
    }

    /// Actions sent, across boots.
    #[inline]
    #[must_use]
    pub const fn spent(&self) -> u64 {
        self.spent_requests
    }

    /// Venue-observed traded notional, USDC 1e6.
    #[inline]
    #[must_use]
    pub const fn traded_1e6(&self) -> i64 {
        self.traded_usdc_1e6
    }

    /// The refusal threshold.
    #[inline]
    #[must_use]
    pub const fn floor(&self) -> u64 {
        self.floor
    }

    /// The address these figures belong to.
    #[inline]
    #[must_use]
    pub const fn address(&self) -> &[u8; 20] {
        &self.address
    }

    /// Serialise to the state-file line. Written by the worker in its
    /// cold block, so a `String` here is not a hot-path allocation.
    #[must_use]
    pub fn to_line(self) -> String {
        format!(
            "{}\t{}\t{}\n",
            crate::config::hex20(&self.address),
            self.spent_requests,
            self.traded_usdc_1e6
        )
    }

    /// Restore from a state-file line, for `address`.
    ///
    /// Returns `None` — meaning "use [`AddressBudget::cold`]" — for a
    /// malformed line OR a line written for a different address.
    /// Inheriting another address's allowance is exactly the mistake
    /// that spends a budget that was never granted.
    #[must_use]
    pub fn from_line(line: &str, address: [u8; 20], floor: u64) -> Option<Self> {
        let mut f = line.trim().split('\t');
        let addr_s = f.next()?;
        let spent: u64 = f.next()?.parse().ok()?;
        // A figure this side of absurd is a corrupt file, not a
        // history. Refusing lands the caller on the cold budget.
        if spent > i64::MAX as u64 {
            return None;
        }
        let traded: i64 = f.next()?.parse().ok()?;
        if f.next().is_some() {
            return None;
        }
        if !addr_s.eq_ignore_ascii_case(&crate::config::hex20(&address)) {
            return None;
        }
        Some(Self::restored(address, floor, spent, traded))
    }
}

/// Default location of the budget's own state file.
pub const DEFAULT_STATE_PATH: &str = "exec-budget.state";

/// Bytes a `userRateLimit` request needs.
pub const MAX_RATE_REQ: usize = 80;

/// Render `{"type":"userRateLimit","user":"0x<40 hex>"}`.
///
/// # Errors
/// `out` is shorter than [`MAX_RATE_REQ`].
pub fn rate_limit_request(
    out: &mut [u8],
    master: &[u8; 20],
) -> Result<usize, crate::response::ScanErr> {
    crate::recon::user_info_request(out, br#"{"type":"userRateLimit","user":"0x"#, master)
}

/// Scan a `userRateLimit` answer into
/// `(nRequestsUsed, cumVlm × 1e6, nRequestsSurplus)`.
///
/// `nRequestsUsed` and `cumVlm` are REQUIRED: an answer missing either
/// is refused, and the caller lands on the file or the cold budget —
/// never on a half-read figure that could authorise spending the venue
/// did not grant. `cumVlm` is a decimal string of USDC.
/// `nRequestsSurplus` (S7-L1) is counted only when the answer ALSO
/// states `nRequestsCap` as exactly the grant plus volume
/// (`10 000 + ⌊cumVlm⌋`) — the netting the venue documents, where
/// reserved weight lives in `nRequestsUsed` / `nRequestsSurplus` and not
/// in the cap. A cap that says otherwise may already carry the reserve,
/// and counting the surplus on top would be the optimistic direction;
/// an absent cap cannot be checked. Either way the surplus reads `0`:
/// headroom not proven is headroom not claimed.
#[must_use]
pub fn scan_rate_limit(body: &[u8]) -> Option<(u64, i64, u64)> {
    let used = crate::json::u64_field(body, b"\"nRequestsUsed\"")?;
    // `decimal_field` is ×1e8; the governor keeps USDC ×1e6.
    let vlm_1e6 = crate::json::decimal_field(body, b"\"cumVlm\"")? / 100;
    let grant = INITIAL_BUFFER.saturating_add(u64::try_from(vlm_1e6 / 1_000_000).unwrap_or(0));
    let surplus = match crate::json::u64_field(body, b"\"nRequestsCap\"") {
        Some(cap) if cap == grant => {
            crate::json::u64_field(body, b"\"nRequestsSurplus\"").unwrap_or(0)
        }
        _ => 0,
    };
    Some((used, vlm_1e6, surplus))
}

/// **S7-L1** — default location of the request-weight top-up's state,
/// beside the budget's.
pub const TOPUP_STATE_PATH: &str = "exec-topup.state";

/// **XMM XH5c-2** — the room one top-up state line takes, written in place
/// ([`TopupState::write_line`]): `0x` and 40 hex, four tabs, four decimals
/// of ≤ 20 characters (the spend's sign included), a newline — ≤ 127 B.
pub const TOPUP_LINE_MAX: usize = 128;
/// The widest line: `0x` and 40 hex, four times a tab and ≤ 20 characters,
/// the newline.
const TOPUP_LINE_WIDEST: usize = crate::config::HEX20_LEN + 4 * (1 + 20) + 1;
// The widest line fits: no figure can carry a write past the buffer.
const _: () = assert!(TOPUP_LINE_WIDEST <= TOPUP_LINE_MAX);

/// A state line's bytes, written in place: `core::fmt` into a fixed
/// buffer, 0 B. A write past it is refused (`fmt::Error`) — the bound
/// above makes that unreachable.
struct LineBuf<'a> {
    buf: &'a mut [u8; TOPUP_LINE_MAX],
    n: usize,
}

impl core::fmt::Write for LineBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.n + s.len();
        if end > TOPUP_LINE_MAX {
            return Err(core::fmt::Error);
        }
        // COPY: `core::fmt`'s pieces (≤ 20 B each: a decimal, a tab, the
        // newline) into the line, ≤ 85 B a line (`TOPUP_LINE_WIDEST` less
        // the master's 42, rendered in place) — cold: once a hand-over on
        // the gateway's thread, and the blocking arm's store; the line is the
        // one buffer the write takes. Rejected: a second integer renderer,
        // for a few stores an hour.
        self.buf[self.n..end].copy_from_slice(s.as_bytes());
        self.n = end;
        Ok(())
    }
}

/// **S7-L1** — what the top-up must remember across a restart: the UTC
/// day and the weight sent on it (the day ceiling), and the session's
/// spend with the anchor it belongs to (the session P&L).
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct TopupState {
    /// UTC day number (`wall_ms / 86 400 000`).
    pub day: u64,
    /// Weight sent on `day`.
    pub used: u64,
    /// `set_unix_s` of the session anchor the spend belongs to; `0` =
    /// none.
    pub anchor_set_s: u64,
    /// The session's top-up spend, USD ×1e6.
    pub cost_1e6: i64,
}

impl TopupState {
    /// The state-file line:
    /// `<0x master>\t<day>\t<used>\t<anchor s>\t<cost ×1e6>\n` — the tests'
    /// spelling of [`Self::write_line`] (one format; every store writes it
    /// in place).
    #[cfg(test)]
    #[must_use]
    pub fn to_line(self, address: &[u8; 20]) -> String {
        let mut line = [0u8; TOPUP_LINE_MAX];
        let n = self.write_line(address, &mut line);
        String::from_utf8_lossy(&line[..n]).into_owned()
    }

    /// **XMM XH5c-2** — the state-file line,
    /// `<0x master>\t<day>\t<used>\t<anchor s>\t<cost ×1e6>\n`, written in
    /// place into `out`: its length. 0 B — slot 6's top-up keeper stores it
    /// on its gateway's thread after init, the blocking arm on its idle
    /// path. The master as [`crate::config::hex20_into`] renders it, into
    /// the line itself.
    pub fn write_line(self, address: &[u8; 20], out: &mut [u8; TOPUP_LINE_MAX]) -> usize {
        use core::fmt::Write as _;
        crate::config::hex20_into(address, out);
        let mut w = LineBuf {
            buf: out,
            n: crate::config::HEX20_LEN,
        };
        let ok = writeln!(
            w,
            "\t{}\t{}\t{}\t{}",
            self.day, self.used, self.anchor_set_s, self.cost_1e6
        )
        .is_ok();
        debug_assert!(
            ok && w.n <= TOPUP_LINE_WIDEST,
            "a top-up line fits {TOPUP_LINE_WIDEST} B"
        );
        w.n
    }

    /// Restore for `address`; `None` for a malformed line, a negative
    /// spend, or a line written for another address.
    #[must_use]
    pub fn from_line(line: &str, address: [u8; 20]) -> Option<Self> {
        let mut f = line.trim().split('\t');
        let addr_s = f.next()?;
        let day: u64 = f.next()?.parse().ok()?;
        let used: u64 = f.next()?.parse().ok()?;
        let anchor_set_s: u64 = f.next()?.parse().ok()?;
        let cost_1e6: i64 = f.next()?.parse().ok()?;
        if f.next().is_some() || cost_1e6 < 0 {
            return None;
        }
        if !addr_s.eq_ignore_ascii_case(&crate::config::hex20(&address)) {
            return None;
        }
        Some(Self {
            day,
            used,
            anchor_set_s,
            cost_1e6,
        })
    }
}

/// **S7-L1** — restore the top-up's `(day, used, cost ×1e6)` for
/// `today` and the anchor set at `anchor_set_s`.
///
/// A MISSING file is a fresh start: nothing sent today, nothing spent
/// this session. Every other failure — unreadable, longer than one line
/// (XMM XH5c-2: read into [`TOPUP_LINE_MAX`] + 1 B, never whole),
/// malformed, another address — reads as today's ceiling already SPENT
/// (`used = u64::MAX`): the arm cannot know what it bought, and the safe
/// answer is to buy nothing until tomorrow. A day stored one LATER than `today`
/// (the clock stepped back across 00:00Z) is kept with its usage — it never
/// re-opens a day already spent (XMM XH5c-1); one stored further ahead (a
/// clock that once ran ahead) reads as today spent, so the next day heals
/// it. The spend carries over only for the anchor it was recorded under; a
/// new session starts at zero.
#[must_use]
pub fn restore_topup(
    path: &std::path::Path,
    address: [u8; 20],
    today: u64,
    anchor_set_s: u64,
) -> (u64, u64, i64) {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (today, 0, 0),
        Err(_) => return (today, u64::MAX, 0),
    };
    let mut buf = [0u8; TOPUP_LINE_MAX + 1];
    let mut n = 0usize;
    while n < buf.len() {
        // COPY: the state file into a 129 B stack buffer at BOOT, once — a
        // file read is a copy; rejected: an unbounded `read_to_string` (a
        // file of any size, a String), a mapping for ≤ 127 B.
        match std::io::Read::read(&mut f, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return (today, u64::MAX, 0),
        }
    }
    let line = if n <= TOPUP_LINE_MAX {
        core::str::from_utf8(&buf[..n]).ok()
    } else {
        None
    };
    let Some(s) = line.and_then(|t| TopupState::from_line(t, address)) else {
        return (today, u64::MAX, 0);
    };
    let (day, used) = if s.day > today.saturating_add(1) {
        (today, u64::MAX)
    } else if s.day >= today {
        (s.day, s.used)
    } else {
        (today, 0)
    };
    let cost = if anchor_set_s != 0 && s.anchor_set_s == anchor_set_s {
        s.cost_1e6
    } else {
        0
    };
    (day, used, cost)
}

/// **S7-L1** — write the top-up state durably (temp file, `sync_all`,
/// rename). After every top-up that left the host: rare, on the idle
/// path, never the tick path. XMM XH5c-2: the line written in place (no
/// `String`); the temp name taken on the call.
///
/// # Errors
/// The temp write, the fsync or the rename failed; the message names
/// the path.
pub fn store_topup(
    path: &std::path::Path,
    address: &[u8; 20],
    s: TopupState,
) -> Result<(), String> {
    let mut line = [0u8; TOPUP_LINE_MAX];
    let n = s.write_line(address, &mut line);
    core_io::write_atomic_at(path, &core_io::tmp_path(path), &line[..n])
}

/// **S7-L1** — the venue's price of one reserved request, USD ×1e6
/// (0.0005 USDC, "paid from the Perps balance").
pub const TOPUP_PRICE_1E6: i64 = 500;

/// **S7-L1 (gap E)** — with the venue's own headroom in hand: buy one
/// top-up of `weight`? Only inside the band `(floor, floor + weight]`
/// — above it there IS headroom (a purchase the arm could not confirm
/// may already have gone through), and at or under the floor the router
/// has halted the slot, sticky — and only while the day's ceiling holds
/// one more. Pure, so the arithmetic is pinned without a venue.
#[inline]
#[must_use]
pub const fn topup_fits(remaining: i64, floor: i64, weight: u64, used: u64, day_max: u64) -> bool {
    let w = if weight > i64::MAX as u64 {
        i64::MAX
    } else {
        weight as i64
    };
    remaining > floor
        && remaining <= floor.saturating_add(w)
        && used.saturating_add(weight) <= day_max
}

/// **The request-weight top-up governor** — S7-L1 (gap E); XMM XH5c: one
/// implementation, both Hyperliquid arms. The purchase policy as state:
/// the weight a top-up buys (`0` = off), the most a UTC day may buy, the
/// day counted, the weight bought on it (or possibly bought), and what this
/// session's top-ups cost. Each arm runs its own round trip — the blocking
/// arm in one idle call (`HlExchange::top_up_budget`), the gateway arm
/// across its budget reads (XMM XH5c-2: it decides by [`Self::take`] and
/// returns a purchase refused before it left by [`Self::refund_day`]) —
/// and both decide by [`Self::wants`], confirm by [`Self::confirms`] and
/// stop by [`Self::stop`]. 40 B, `Copy`; never allocates.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct TopupGovernor {
    weight: u64,
    day_max: u64,
    day: u64,
    used: u64,
    cost_1e6: i64,
}

const _: () = assert!(core::mem::size_of::<TopupGovernor>() == 40);

impl TopupGovernor {
    /// Off (`weight` 0), with the day's usage and the session's spend a
    /// restart restored ([`restore_topup`]).
    #[must_use]
    pub const fn restored(day: u64, used: u64, cost_1e6: i64) -> Self {
        Self {
            weight: 0,
            day_max: 0,
            day,
            used,
            cost_1e6,
        }
    }

    /// Armed: buy `weight` per top-up, at most `day_max` a UTC day
    /// (`weight` 0 leaves it off); the restored day, usage and spend kept.
    #[must_use]
    pub const fn armed(self, weight: u64, day_max: u64) -> Self {
        Self {
            weight,
            day_max,
            ..self
        }
    }

    /// The weight one top-up buys; `0` = off.
    #[inline]
    #[must_use]
    pub const fn weight(&self) -> u64 {
        self.weight
    }

    /// This session's top-up spend, USD ×1e6.
    #[inline]
    #[must_use]
    pub const fn cost_1e6(&self) -> i64 {
        self.cost_1e6
    }

    /// XMM XH5c-2: the weight bought — or possibly bought — on the day
    /// counted (`u64::MAX`: a restore that could not read it, today spent).
    #[inline]
    #[must_use]
    pub const fn used(&self) -> u64 {
        self.used
    }

    /// XMM XH5c-2: the most a UTC day may buy (the boot's tell).
    #[inline]
    #[must_use]
    pub const fn day_max(&self) -> u64 {
        self.day_max
    }

    /// The state line's figures, tied to the anchor set at `anchor_set_s`.
    #[must_use]
    pub const fn state(&self, anchor_set_s: u64) -> TopupState {
        TopupState {
            day: self.day,
            used: self.used,
            anchor_set_s,
            cost_1e6: self.cost_1e6,
        }
    }

    /// Above the band — headroom no top-up is for: `remaining` over `floor`
    /// plus one top-up's weight. The blocking arm's check before it reads
    /// the venue; the band rule itself is [`topup_fits`].
    #[must_use]
    pub const fn above_band(&self, remaining: i64, floor: u64) -> bool {
        let f = if floor > i64::MAX as u64 {
            i64::MAX
        } else {
            floor as i64
        };
        let w = if self.weight > i64::MAX as u64 {
            i64::MAX
        } else {
            self.weight as i64
        };
        remaining > f.saturating_add(w)
    }

    /// **XMM XH5c-2** — count from `today` when it is a LATER UTC day (its
    /// usage from 0) — forward only: a clock stepping back across 00:00Z
    /// never re-opens a day already spent. [`Self::wants`] rolls first; the
    /// gateway arm rolls before a stop, so a stop spends the day it happens
    /// on.
    pub fn roll(&mut self, today: u64) {
        if today > self.day {
            self.day = today;
            self.used = 0;
        }
    }

    /// Buy one now? Off never buys. The day rolled ([`Self::roll`]); then
    /// the band rule ([`topup_fits`]) on the venue's own `remaining` over
    /// `floor`, inside the day's max.
    pub fn wants(&mut self, remaining: i64, floor: u64, today: u64) -> bool {
        if self.weight == 0 {
            return false;
        }
        self.roll(today);
        let floor = i64::try_from(floor).unwrap_or(i64::MAX);
        topup_fits(remaining, floor, self.weight, self.used, self.day_max)
    }

    /// Decide AND charge the day in one step — for an arm whose purchase
    /// answers later (XMM XH5c-2's gateway arm): `true` = buy one, its
    /// weight already counted against the day, so two decisions before an
    /// answer can never both pass the day's max.
    pub fn take(&mut self, remaining: i64, floor: u64, today: u64) -> bool {
        let yes = self.wants(remaining, floor, today);
        if yes {
            self.charge_day();
        }
        yes
    }

    /// One top-up bought, or possibly bought: the day's usage charged.
    pub fn charge_day(&mut self) {
        self.used = self.used.saturating_add(self.weight);
    }

    /// **XMM XH5c-2** — the day's charge of a purchase [`Self::take`]
    /// decided that never left the host (refused unsent: halted, not
    /// ready, throttled; a full ring; abandoned before it left) — never
    /// bought, so returned: a throttled minute does not spend the day's
    /// max on nothing. Stopped, it returns nothing (the weight is 0).
    pub fn refund_day(&mut self) {
        self.used = self.used.saturating_sub(self.weight);
    }

    /// One top-up's price charged to this session's spend.
    pub fn charge_cost(&mut self) {
        let w = i64::try_from(self.weight).unwrap_or(i64::MAX);
        self.cost_1e6 = self
            .cost_1e6
            .saturating_add(w.saturating_mul(TOPUP_PRICE_1E6));
    }

    /// **Seen on the venue, or no more:** does `after` — the venue's
    /// headroom read after a purchase — show it over `before`, less the
    /// request it cost (half the weight is the bar)? A purchase that does
    /// not show may be credited where the arm cannot read it: the next one
    /// could buy the same need twice.
    #[must_use]
    pub const fn confirms(&self, before: i64, after: i64) -> bool {
        let w = if self.weight > i64::MAX as u64 {
            i64::MAX
        } else {
            self.weight as i64
        };
        after >= before.saturating_add(w / 2)
    }

    /// No more top-ups from this process — nor, for the rest of the UTC
    /// day, from the next: the day reads as spent once its state is stored.
    pub fn stop(&mut self) {
        self.weight = 0;
        self.used = self.used.max(self.day_max);
    }

    /// A new anchor: its session's spend starts at zero.
    pub fn new_session(&mut self) {
        self.cost_1e6 = 0;
    }
}

/// Read the budget for `address` from `path`.
///
/// **Every failure returns a COLD budget**, which is the conservative
/// one: a missing file, an unreadable file, a malformed line and a
/// line written for a different address all mean "we do not know what
/// this address has spent", and the only safe answer to that is to
/// assume it has spent its grant. Returning a permissive default on a
/// read error is how a governor comes to authorise the very spending
/// it exists to prevent.
#[must_use]
pub fn load(path: &std::path::Path, address: [u8; 20], floor: u64) -> AddressBudget {
    // COPY: one ≤ 100 B state line read into a String at BOOT — the
    // restore path, once per process; never the tick path.
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| AddressBudget::from_line(&t, address, floor))
        .unwrap_or_else(|| AddressBudget::cold(address, floor))
}

/// Write the budget durably (temp file, `sync_all`, rename).
///
/// Called from the worker's 5 s cold block, never the tick path.
///
/// # Errors
/// The temp write, the fsync or the rename failed; the message names
/// the path.
pub fn store(path: &std::path::Path, b: &AddressBudget) -> Result<(), String> {
    core_io::write_atomic(path, &b.to_line())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: [u8; 20] = [0xAB; 20];
    const OTHER: [u8; 20] = [0xCD; 20];

    /// The formula from the plan, and the direction each term moves.
    #[test]
    fn the_allowance_is_the_grant_plus_volume_minus_spend() {
        let mut b = AddressBudget::restored(ADDR, 100, 0, 0);
        assert_eq!(b.remaining(), 10_000);

        b.on_action_sent(1);
        assert_eq!(b.remaining(), 9_999, "an action costs one");

        // $1 of venue volume earns exactly one request.
        b.on_venue_fill(1_000_000);
        assert_eq!(b.remaining(), 10_000);

        // Sub-dollar fills earn nothing until they add up — integer
        // division, deliberately, because rounding UP would invent
        // allowance the venue did not grant.
        let mut b = AddressBudget::restored(ADDR, 0, 0, 0);
        for _ in 0..9 {
            b.on_venue_fill(100_000); // $0.10
        }
        assert_eq!(b.remaining(), 10_000, "$0.90 earns nothing");
        b.on_venue_fill(100_000);
        assert_eq!(b.remaining(), 10_001, "$1.00 earns one");
    }

    /// A sell is volume. Counting only buys would drift optimistic.
    #[test]
    fn both_sides_count_as_volume() {
        let mut b = AddressBudget::restored(ADDR, 0, 0, 0);
        b.on_venue_fill(-5_000_000);
        assert_eq!(b.traded_1e6(), 5_000_000);
        assert_eq!(b.remaining(), 10_005);
    }

    /// E7-F1: the venue's own answer, verbatim from mainnet and
    /// testnet on 2026-09-19, becomes the budget the arm starts from —
    /// and `remaining()` reproduces the venue's `cap − used`.
    #[test]
    fn the_venue_states_the_budget_and_remaining_is_its_own_arithmetic() {
        const MAINNET: &[u8] =
            br#"{"cumVlm":"0.0","nRequestsUsed":5,"nRequestsCap":10000,"nRequestsSurplus":0}"#;
        const TESTNET: &[u8] =
            br#"{"cumVlm":"42.0","nRequestsUsed":95,"nRequestsCap":10042,"nRequestsSurplus":0}"#;
        let (used, vlm, surplus) = scan_rate_limit(MAINNET).expect("mainnet shape");
        assert_eq!((used, vlm, surplus), (5, 0, 0));
        let b = AddressBudget::from_venue(ADDR, 2000, used, vlm, surplus);
        assert_eq!(b.remaining(), 10_000 - 5);
        assert!(b.may_submit().is_ok(), "9 995 is over any sane floor");

        let (used, vlm, surplus) = scan_rate_limit(TESTNET).expect("testnet shape");
        assert_eq!((used, vlm, surplus), (95, 42_000_000, 0));
        let b = AddressBudget::from_venue(ADDR, 2000, used, vlm, surplus);
        assert_eq!(b.remaining(), 10_042 - 95, "cap − used, as the venue says");

        // S7-L1: the surplus is optional on the wire and reads 0 absent.
        let bare = br#"{"cumVlm":"1.0","nRequestsUsed":7}"#;
        assert_eq!(scan_rate_limit(bare), Some((7, 1_000_000, 0)));
        // ...and counts only beside a cap that is exactly the grant plus
        // volume (the docs' own example: 10 000 + 2 854 574).
        let docs = br#"{"cumVlm":"2854574.593578","nRequestsUsed":2890,"nRequestsCap":2864574,"nRequestsSurplus":7}"#;
        assert_eq!(scan_rate_limit(docs).map(|t| t.2), Some(7));
        let carried =
            br#"{"cumVlm":"0.0","nRequestsUsed":0,"nRequestsCap":15000,"nRequestsSurplus":5000}"#;
        assert_eq!(
            scan_rate_limit(carried).map(|t| t.2),
            Some(0),
            "a cap that carries it"
        );
        let uncapped = br#"{"cumVlm":"0.0","nRequestsUsed":0,"nRequestsSurplus":5000}"#;
        assert_eq!(
            scan_rate_limit(uncapped).map(|t| t.2),
            Some(0),
            "unprovable"
        );

        // Half an answer is no answer.
        assert!(scan_rate_limit(br#"{"nRequestsUsed":5}"#).is_none());
        assert!(scan_rate_limit(br#"{"cumVlm":"1.0"}"#).is_none());
        assert!(scan_rate_limit(b"[]").is_none());

        // The request the figures come from.
        let mut buf = [0u8; MAX_RATE_REQ];
        let n = rate_limit_request(&mut buf, &ADDR).expect("fits");
        assert_eq!(
            &buf[..n],
            br#"{"type":"userRateLimit","user":"0xabababababababababababababababababababab"}"#
        );
        let mut tiny = [0u8; 8];
        assert!(rate_limit_request(&mut tiny, &ADDR).is_err());
    }

    /// **S7-L1 — reserved weight is headroom, in both of the venue's
    /// regimes.** Reserved beyond use, the venue reports `used = 0` and
    /// the rest as `nRequestsSurplus`; used beyond the reserve, it nets
    /// the reserve out of `used`. Either way `remaining()` is
    /// `cap − used + reserved`, and a reserve accepted mid-session adds
    /// exactly its weight.
    #[test]
    fn reserved_weight_is_headroom_in_both_of_the_venues_regimes() {
        // 10 000 cap, 3 000 used, 5 000 reserved: surplus 2 000.
        let over =
            br#"{"cumVlm":"0.0","nRequestsUsed":0,"nRequestsCap":10000,"nRequestsSurplus":2000}"#;
        let (used, vlm, surplus) = scan_rate_limit(over).expect("shape");
        let b = AddressBudget::from_venue(ADDR, 2000, used, vlm, surplus);
        assert_eq!(b.remaining(), 10_000 - 3_000 + 5_000);

        // 10 000 cap, 9 000 used, 5 000 reserved: used nets to 4 000.
        let under =
            br#"{"cumVlm":"0.0","nRequestsUsed":4000,"nRequestsCap":10000,"nRequestsSurplus":0}"#;
        let (used, vlm, surplus) = scan_rate_limit(under).expect("shape");
        let mut b = AddressBudget::from_venue(ADDR, 2000, used, vlm, surplus);
        assert_eq!(b.remaining(), 10_000 - 9_000 + 5_000);

        // A reserve accepted now adds its weight; the action that
        // bought it is one request like any other.
        b.on_action_sent(1);
        b.on_reserved(5_000);
        assert_eq!(b.remaining(), 6_000 - 1 + 5_000);
        assert!(b.may_submit().is_ok());
    }

    /// **S7-L1 — the top-up's state.** The line round-trips for its own
    /// address only; a missing file is a fresh start; a garbled or
    /// foreign one reads as the day's ceiling spent; the day's usage
    /// carries only within its day and the spend only under its anchor.
    #[test]
    fn the_topup_state_restores_conservatively() {
        let s = TopupState {
            day: 20_720,
            used: 10_000,
            anchor_set_s: 1_790_200_000,
            cost_1e6: 5_000_000,
        };
        let line = s.to_line(&ADDR);
        assert_eq!(
            line,
            "0xabababababababababababababababababababab\t20720\t10000\t1790200000\t5000000\n"
        );
        assert_eq!(TopupState::from_line(&line, ADDR), Some(s));
        assert_eq!(TopupState::from_line(&line, OTHER), None);
        assert_eq!(
            TopupState::from_line("0xab\t1\t2\t3\t-4", ADDR),
            None,
            "negative"
        );

        let dir = std::env::temp_dir().join(format!("mv-topup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(TOPUP_STATE_PATH);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            restore_topup(&path, ADDR, 20_720, 1_790_200_000),
            (20_720, 0, 0),
            "fresh"
        );

        store_topup(&path, &ADDR, s).expect("stores");
        assert_eq!(
            restore_topup(&path, ADDR, 20_720, 1_790_200_000),
            (20_720, 10_000, 5_000_000),
            "same day, same session"
        );
        assert_eq!(
            restore_topup(&path, ADDR, 20_721, 1_790_200_000),
            (20_721, 0, 5_000_000)
        );
        assert_eq!(
            restore_topup(&path, ADDR, 20_719, 1_790_200_000),
            (20_720, 10_000, 5_000_000),
            "a clock stepped back keeps the later day and its usage"
        );
        assert_eq!(
            restore_topup(&path, ADDR, 20_718, 1_790_200_000),
            (20_718, u64::MAX, 5_000_000),
            "a day further ahead: today spent, tomorrow healed"
        );
        assert_eq!(
            restore_topup(&path, ADDR, 20_720, 1_790_300_000),
            (20_720, 10_000, 0)
        );
        assert_eq!(
            restore_topup(&path, ADDR, 20_720, 0),
            (20_720, 10_000, 0),
            "unanchored"
        );
        assert_eq!(
            restore_topup(&path, OTHER, 20_720, 1),
            (20_720, u64::MAX, 0),
            "foreign"
        );
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(
            restore_topup(&path, ADDR, 20_720, 1),
            (20_720, u64::MAX, 0),
            "garbled"
        );
        // XMM XH5c-2: read into one line's room, never whole — a longer file
        // (a valid line padded past it) is no state.
        let long = format!(
            "{}{}",
            s.to_line(&ADDR).trim_end(),
            " ".repeat(TOPUP_LINE_MAX)
        );
        std::fs::write(&path, long).unwrap();
        assert_eq!(
            restore_topup(&path, ADDR, 20_720, 1),
            (20_720, u64::MAX, 0),
            "overlong"
        );
        store_topup(&path, &ADDR, s).expect("stores");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            s.to_line(&ADDR),
            "the store writes the line in place"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// XMM XH5c-2: the line written in place is the state line, byte for
    /// byte — the widest figures included — and reads back.
    #[test]
    fn the_line_written_in_place_is_the_state_line() {
        let wide = TopupState {
            day: u64::MAX,
            used: u64::MAX,
            anchor_set_s: u64::MAX,
            cost_1e6: i64::MIN,
        };
        let mut line = [0u8; TOPUP_LINE_MAX];
        let n = wide.write_line(&[0xFF; 20], &mut line);
        assert_eq!(n, TOPUP_LINE_WIDEST, "the widest line fits");
        assert_eq!(
            &line[..n],
            format!(
                "0x{}\t{}\t{}\t{}\t{}\n",
                "ff".repeat(20),
                u64::MAX,
                u64::MAX,
                u64::MAX,
                i64::MIN
            )
            .as_bytes()
        );
        let s = TopupState {
            day: 20_720,
            used: 10_000,
            anchor_set_s: 1_790_200_000,
            cost_1e6: 5_000_000,
        };
        let n = s.write_line(&ADDR, &mut line);
        assert_eq!(&line[..n], s.to_line(&ADDR).as_bytes());
        let text = core::str::from_utf8(&line[..n]).expect("ASCII");
        assert_eq!(TopupState::from_line(text, ADDR), Some(s));
    }

    /// **A cold boot must not hand itself the allowance** — the
    /// FALLBACK when the venue does not answer at boot (E7-F1).
    #[test]
    fn a_cold_boot_assumes_the_worst() {
        let b = AddressBudget::cold(ADDR, 100);
        assert_eq!(b.remaining(), 0, "no state file means no headroom");
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
        // …and it recovers only as venue fills accumulate.
        let mut b = b;
        b.on_venue_fill(500_000_000); // $500
        assert_eq!(b.remaining(), 500);
        assert!(b.may_submit().is_ok());
    }

    /// The floor refuses submits and NEVER refuses a cancel.
    #[test]
    fn the_floor_never_strands_a_position() {
        let b = AddressBudget::restored(ADDR, 2_000, 8_500, 0);
        assert_eq!(b.remaining(), 1_500);
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
        assert!(b.may_cancel(), "a halted engine must be able to flatten");

        // Exactly at the floor is refused: the floor is headroom to
        // KEEP, not headroom to spend down to.
        let b = AddressBudget::restored(ADDR, 2_000, 8_000, 0);
        assert_eq!(b.remaining(), 2_000);
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));

        let b = AddressBudget::restored(ADDR, 2_000, 7_999, 0);
        assert!(b.may_submit().is_ok());
    }

    /// Overspend reports how far past the cliff we are, rather than
    /// saturating and losing the distance.
    #[test]
    fn overspend_is_visible_rather_than_clamped() {
        let b = AddressBudget::restored(ADDR, 0, 11_000, 0);
        assert_eq!(b.remaining(), -1_000);
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
    }

    /// **A state file for a different address is not this address's
    /// history.** Inheriting it would spend an allowance never granted.
    #[test]
    fn a_state_file_for_another_address_is_refused() {
        let b = AddressBudget::restored(OTHER, 100, 500, 7_000_000);
        let line = b.to_line();
        assert!(AddressBudget::from_line(&line, OTHER, 100).is_some());
        assert!(
            AddressBudget::from_line(&line, ADDR, 100).is_none(),
            "another address's budget was inherited"
        );
    }

    #[test]
    fn the_state_line_round_trips_and_refuses_junk() {
        let b = AddressBudget::restored(ADDR, 250, 1_234, -9_876);
        let line = b.to_line();
        let back = AddressBudget::from_line(&line, ADDR, 250).expect("round trip");
        assert_eq!(back, b);
        // Trailing newline, whitespace, and case are tolerated.
        assert!(AddressBudget::from_line(&format!("  {}  ", line.trim()), ADDR, 250).is_some());
        assert!(AddressBudget::from_line(&line.to_uppercase(), ADDR, 250).is_some());
        // Junk is refused rather than half-parsed into a budget.
        for bad in [
            "",
            "\t\t",
            "0xabab\t1\t2", // wrong-length address
            &format!("{}\tx\t2", crate::config::hex20(&ADDR)),
            &format!("{}\t1", crate::config::hex20(&ADDR)),
            &format!("{}\t1\t2\t3", crate::config::hex20(&ADDR)),
        ] {
            assert!(
                AddressBudget::from_line(bad, ADDR, 250).is_none(),
                "accepted junk: {bad:?}"
            );
        }
    }

    /// Cache-line sized and Copy — it sits on the worker's stack and
    /// is mirrored, never shared mutably.
    #[test]
    fn the_layout_is_one_cache_line() {
        assert_eq!(core::mem::size_of::<AddressBudget>(), 64);
        assert_eq!(core::mem::align_of::<AddressBudget>(), 64);
    }

    /// Every read failure must land on the CONSERVATIVE budget, not a
    /// permissive one. This is the property the governor rests on.
    #[test]
    fn every_unreadable_state_lands_on_the_cold_budget() {
        let dir = std::env::temp_dir().join(format!("mv-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("exec-budget.state");

        // Absent.
        assert_eq!(load(&p, ADDR, 100), AddressBudget::cold(ADDR, 100));
        // Empty, truncated, junk, and a line for ANOTHER address.
        for bad in [
            String::new(),
            "\n".to_owned(),
            "garbage".to_owned(),
            AddressBudget::restored(OTHER, 100, 5, 5).to_line(),
        ] {
            std::fs::write(&p, &bad).unwrap();
            assert_eq!(
                load(&p, ADDR, 100),
                AddressBudget::cold(ADDR, 100),
                "permissive budget from {bad:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A good file round-trips through the durable write, and the temp
    /// file is not left behind under a name that belongs to no file.
    #[test]
    fn a_stored_budget_comes_back_intact() {
        let dir = std::env::temp_dir().join(format!("mv-budget-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("exec-budget.state");

        let b = AddressBudget::restored(ADDR, 250, 4_242, 77_000_000);
        store(&p, &b).expect("store");
        assert_eq!(load(&p, ADDR, 250), b);

        assert!(!core_io::tmp_path(&p).exists(), "temp left behind");
        // The old, wrong temp name must not be created either.
        assert!(!p.with_extension("tsv.tmp").exists());

        // A rewrite REPLACES rather than appends.
        let b2 = AddressBudget::restored(ADDR, 250, 4_243, 78_000_000);
        store(&p, &b2).expect("store");
        assert_eq!(load(&p, ADDR, 250), b2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// XH4e: the cancel allowance is the smaller of the cap plus 100 000
    /// and twice the cap, less what the address spent — past the order
    /// cliff, cancels still have room.
    #[test]
    fn the_cancel_allowance_is_the_smaller_of_cap_plus_100k_and_twice_cap() {
        let b = AddressBudget::from_venue(ADDR, 0, 12_000, 5_000_000_000, 0);
        // cap = 10 000 + 5 000 = 15 000: twice it (30 000) is the smaller.
        assert_eq!(b.remaining(), 3_000);
        assert_eq!(b.cancel_remaining(), 18_000);
        let b = AddressBudget::from_venue(ADDR, 0, 250_000, 190_000_000_000, 0);
        // cap = 200 000: plus 100 000 is the smaller.
        assert_eq!(b.remaining(), -50_000, "past the order cliff");
        assert_eq!(b.cancel_remaining(), 50_000, "cancels still go");
        let b = AddressBudget::restored(ADDR, 0, u64::MAX, i64::MAX);
        assert!(b.cancel_remaining() <= 0, "no wrap");
    }

    /// XH4e: a request refused unsent is taken back — never below zero.
    #[test]
    fn a_request_that_never_left_is_taken_back() {
        let mut b = AddressBudget::from_venue(ADDR, 0, 500, 0, 0);
        b.on_action_sent(3);
        b.on_action_unsent(1);
        assert_eq!(b.spent(), 502);
        let mut z = AddressBudget::from_venue(ADDR, 0, 0, 0, 0);
        z.on_action_unsent(5);
        assert_eq!(z.spent(), 0, "saturating");
    }

    /// XH4e: the budget read's scan runs every `limit_ms` on the gateway's
    /// steady path — every truncation of a good answer and 20 000 random
    /// ones return, never panic.
    #[test]
    fn the_rate_limit_scan_never_panics_on_arbitrary_bytes() {
        let good = br#"{"cumVlm":"2854574.593578","nRequestsUsed":2890,"nRequestsCap":2864574,"nRequestsSurplus":0}"#;
        assert!(scan_rate_limit(good).is_some());
        for k in 0..good.len() {
            let _ = scan_rate_limit(&good[..k]);
        }
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..20_000 {
            let mut buf = [0u8; 96];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            let _ = scan_rate_limit(&buf);
        }
    }

    /// Hostile figures from a venue row must not panic the fill path.
    #[test]
    fn extreme_inputs_saturate_rather_than_panic() {
        let mut b = AddressBudget::restored(ADDR, 0, 0, i64::MAX);
        b.on_venue_fill(i64::MAX);
        assert_eq!(b.traded_1e6(), i64::MAX);
        let _ = b.remaining();

        // THE WRAP. `spent as i64` would make this report +10_001 —
        // a maximally-spent address claiming near-full headroom.
        let b = AddressBudget::restored(ADDR, 0, u64::MAX, 0);
        assert!(
            b.remaining() <= 0,
            "a maximally-spent address reported {} headroom",
            b.remaining()
        );
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
        let mut b = b;
        b.on_action_sent(1);
        assert_eq!(b.spent(), u64::MAX);
        assert!(b.remaining() <= 0);

        // …and such a figure in a state FILE is refused outright, so
        // the caller lands cold rather than restoring nonsense.
        let line = format!("{}\t{}\t0", crate::config::hex20(&ADDR), u64::MAX);
        assert!(AddressBudget::from_line(&line, ADDR, 0).is_none());

        let mut b = AddressBudget::restored(ADDR, 0, 0, 0);
        b.on_venue_fill(i64::MIN);
        let _ = b.remaining();
    }

    /// **S7-L1 (gap E) — the top-up buys only inside its band and
    /// under its ceiling.** Floor 2 000, weight 5 000, ceiling 30 000.
    #[test]
    fn a_topup_buys_only_between_the_floor_and_one_topup_above_it() {
        assert!(
            topup_fits(7_000, 2_000, 5_000, 0, 30_000),
            "the top of the band"
        );
        assert!(
            topup_fits(2_001, 2_000, 5_000, 0, 30_000),
            "just over the floor"
        );
        assert!(
            !topup_fits(7_001, 2_000, 5_000, 0, 30_000),
            "headroom: nothing to buy"
        );
        assert!(
            !topup_fits(2_000, 2_000, 5_000, 0, 30_000),
            "at the floor: halted, sticky"
        );
        assert!(!topup_fits(-50, 2_000, 5_000, 0, 30_000), "past the cliff");
        assert!(
            topup_fits(5_000, 2_000, 5_000, 25_000, 30_000),
            "the day's last one"
        );
        assert!(
            !topup_fits(5_000, 2_000, 5_000, 25_001, 30_000),
            "over the ceiling"
        );
        assert!(
            !topup_fits(5_000, 2_000, 5_000, u64::MAX, 30_000),
            "an unreadable day"
        );
        // Saturates rather than wrapping: a weight past i64 reads as the
        // widest band, never as a negative one.
        assert!(topup_fits(5_000, 2_000, u64::MAX, 0, u64::MAX));
        assert!(
            !topup_fits(5_000, 2_000, u64::MAX, 1, u64::MAX - 1),
            "and the ceiling holds"
        );
    }

    /// XMM XH5c: the governor — off buys nothing; armed, it buys inside the
    /// band and the day's max, rolls the day, charges the day and the
    /// session, confirms on half the weight, and a stop spends the day.
    #[test]
    fn the_governor_buys_in_its_band_charges_confirms_and_stops() {
        let restored = TopupGovernor::restored(19_999, 25_000, 7);
        let mut off = restored;
        assert!(!off.wants(5_000, 2_000, 19_999), "off");
        let mut g = restored.armed(5_000, 30_000);
        assert_eq!((g.weight(), g.cost_1e6()), (5_000, 7));
        assert!(!g.wants(7_001, 2_000, 19_999), "above the band");
        assert!(
            g.wants(5_000, 2_000, 19_999),
            "in the band, the day's last one"
        );
        g.charge_day();
        assert!(!g.wants(5_000, 2_000, 19_999), "the day's max");
        assert!(g.wants(5_000, 2_000, 20_000), "a new day counts from zero");
        g.charge_cost();
        assert_eq!(g.cost_1e6(), 7 + 5_000 * TOPUP_PRICE_1E6);
        assert_eq!(
            g.state(42),
            TopupState {
                day: 20_000,
                used: 0,
                anchor_set_s: 42,
                cost_1e6: 2_500_007,
            }
        );
        assert!(g.confirms(3_000, 5_500), "half the weight shows");
        assert!(!g.confirms(3_000, 5_499), "less does not");
        assert!(g.above_band(7_001, 2_000) && !g.above_band(7_000, 2_000));
        let mut t = TopupGovernor::restored(20_000, 25_000, 0).armed(5_000, 30_000);
        assert!(t.take(5_000, 2_000, 20_000), "decided and charged at once");
        assert!(
            !t.take(5_000, 2_000, 20_000),
            "a second decision before an answer: the day's max"
        );
        assert_eq!(t.state(0).used, 30_000);
        assert!(
            !t.wants(5_000, 2_000, 19_999),
            "a clock stepped back re-opens nothing"
        );
        assert_eq!(t.state(0).day, 20_000);
        g.new_session();
        assert_eq!(g.cost_1e6(), 0);
        g.stop();
        assert_eq!(g.weight(), 0);
        assert_eq!(g.state(0).used, 30_000, "the day reads as spent");
        assert!(!g.wants(5_000, 2_000, 20_000), "stopped");
    }

    /// XMM XH5c-2: the day rolls forward only — a later day counts from 0,
    /// an earlier one changes nothing — and a stop after a roll spends the
    /// new day.
    #[test]
    fn the_day_rolls_forward_only_and_a_stop_spends_the_day_it_happens_on() {
        let mut g = TopupGovernor::restored(20_000, 7_000, 0).armed(5_000, 30_000);
        g.roll(19_999);
        assert_eq!(
            (g.state(0).day, g.used()),
            (20_000, 7_000),
            "backward: nothing"
        );
        g.roll(20_001);
        assert_eq!((g.state(0).day, g.used()), (20_001, 0), "forward: from 0");
        g.stop();
        assert_eq!((g.state(0).day, g.used()), (20_001, 30_000), "today spent");
        assert_eq!((g.day_max(), g.weight()), (30_000, 0));
    }

    /// XMM XH5c-2: the headroom apart from volume — a purchase moves it,
    /// traded volume does not.
    #[test]
    fn the_unearned_headroom_moves_with_a_purchase_and_not_with_volume() {
        let mut b = AddressBudget::from_venue(ADDR, 2_000, 7_400, 0, 0);
        assert_eq!((b.remaining(), b.remaining_unearned()), (2_600, 2_600));
        b.on_venue_fill(5_000 * USDC_PER_REQUEST_1E6);
        assert_eq!(
            (b.remaining(), b.remaining_unearned()),
            (7_600, 2_600),
            "volume"
        );
        b.on_reserved(1_000);
        assert_eq!(
            (b.remaining(), b.remaining_unearned()),
            (8_600, 3_600),
            "a purchase"
        );
        let v = AddressBudget::from_venue(ADDR, 2_000, 7_400, 1_000 * USDC_PER_REQUEST_1E6, 1_000);
        assert_eq!((v.remaining(), v.remaining_unearned()), (4_600, 3_600));
    }

    /// XMM XH5c-2: a purchase decided that never left returns the day's
    /// charge — the next decision passes again — and a stopped governor
    /// returns nothing.
    #[test]
    fn a_purchase_that_never_left_returns_the_day_s_charge() {
        let mut g = TopupGovernor::restored(20_000, 0, 0).armed(5_000, 5_000);
        assert!(g.take(5_000, 2_000, 20_000));
        assert_eq!(g.used(), 5_000);
        assert!(!g.take(5_000, 2_000, 20_000), "the day's max");
        g.refund_day();
        assert_eq!(g.used(), 0, "never bought");
        assert!(g.take(5_000, 2_000, 20_000), "decided again");
        g.stop();
        g.refund_day();
        assert_eq!(g.used(), 5_000, "stopped: nothing returned");
        let mut z = TopupGovernor::restored(20_000, 0, 0).armed(5_000, 5_000);
        z.refund_day();
        assert_eq!(z.used(), 0, "saturating");
    }
}
