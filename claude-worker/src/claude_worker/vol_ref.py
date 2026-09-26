# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Integer volatility-forecast reference law (VRP V4; Rust counterpart:
``crates/core-vol``).

The worker's half of the parity contract. ``crates/core-vol`` runs this
exact arithmetic inside the engine; this module runs it over history in
``candles.db`` to cut the boot seed (V5) and to fit offline. A committed
fixture (``tests/fixtures/vol/``) pins the two implementations bit for
bit, so a drift on either side is a red test rather than a silent
disagreement between the number the engine trades on and the number the
research says it should.

Everything is integer. There are exactly three uses of a float in this
file, all of them at import time, all of them building a constant table
that Rust builds from the same expression: ``LOG2_TAB``, ``EXP2_TAB`` and
nothing else. No float ever touches a value derived from market data.

Mirrored operations, and why each is bit-exact across the two languages:

- ``ret_bps_1e9``: ``(to - frm) * 10**13 // frm``. Python ``//`` floors
  toward -inf; Rust uses ``i128::div_euclid``, which is the same for a
  positive divisor, and the divisor is a price.
- ``isqrt``: ``math.isqrt`` is the exact floor root; Rust's Newton
  iteration converges to the same floor.
- The HAR fold, the OLS closed form and QLIKE: pure integer, written in
  the same order of operations as the Rust so intermediate truncation
  lands identically.

The law itself (all scalings pinned, ``docs/research`` build card 2.3)::

    r_k      = ret_bps_1e9(prev, cur)                     bps x 1e9
    rv_W     = isqrt(sum r_k^2)                        W in {15,60,240,1440}
    har_tau  = isqrt(sum_{w >= first_window}(rv_w^2/w) / terms * tau_min)
               4 h / 8 h: first_window 1, terms 3 -- the three-window law
               unchanged to the bit. 15 m: first_window 0, terms 4.
    x        = ln(har_tau)                                x 1e9
    y        = ln(realised vol over the hold)             x 1e9, same domain
               formed by realised_since_arm_1e9 / realised_rv_1e9 -- the
               hold's OWN returns -- and never by har_1e9, a forecast (F1)
    b        = sum(dx*dy) * 1e9 / sum(dx*dx)              x 1e9
    a        = ybar - b*xbar/1e9                          x 1e9
    ln_sig   = a + b*x/1e9                                x 1e9
    iv_lo/hi = exp(ln_sig -/+ theta) * ANNUALISE / 1e13   annualised fraction x 1e9
"""

import math

# --- constants (mirrors of core_vol) -------------------------------

MINUTE_RING = 1536
PAIR_RING = 128
MIN_PAIRS = 60
#: BIN15 P1b (F5): the OLS slope bound and the regressor-spread floor,
#: mirroring ``core_vol::{B_MIN_1E9, B_MAX_1E9, X_SPREAD_MIN_1E9}``. A
#: negative slope inverts the forecast; an unbounded one exponentiates
#: into a sigma the tape never supported; a regressor that never varied
#: is division by nearly nothing. All three hold instead.
B_MIN_1E9 = 0
B_MAX_1E9 = 2_000_000_000
X_SPREAD_MIN_1E9 = 10_000_000
QLIKE_RING = 60
#: BIN15 O4a appended the 15-minute term at the FRONT. Which windows a
#: tenor folds is its ``first_window``, not this tuple's length: 4 h and
#: 8 h start at index 1 and fold the same three terms they always did,
#: so their forecast is bit-identical (the standing ``parity-1`` fixture
#: is the proof -- it was never regenerated). Only 15 m folds all four.
HAR_WINDOWS = (15, 60, 240, 1440)

#: Minute closes the HAR needs before it can forecast at all — mirrors
#: ``core_vol::HAR_WARM_MINUTES``, which is the longest HAR window.
#:
#: Operationally load-bearing, not a detail: the engine's restart lane
#: fires five times a UTC day with a longest gap of 7 h 35 m, so an
#: engine warming only from its own uptime can never reach it. That is
#: what kept the first live campaign from trading.
HAR_WARM_MINUTES = max(HAR_WINDOWS)

BPS_1E9_PER_UNIT = 10_000_000_000_000

LN2_1E9 = 693_147_181
LOG2_1E9_1E9 = 29_897_352_854
LOG2_UNDEFINED = -(2**63)

ANNUALISE_4H_1E9 = 46_813_459_603
ANNUALISE_8H_1E9 = 33_102_114_736
#: ``sqrt(525_960 / 15) * 1e9`` -- the 15 m annualiser (BIN15 O4a), out
#: of the same 525 960-minute year as the two above and rounded the same
#: way (``...411.93`` -> ``...412``).
ANNUALISE_15M_1E9 = 187_253_838_412

TAU_15M_NS = 900_000_000_000
TAU_4H_NS = 14_400_000_000_000
TAU_8H_NS = 28_800_000_000_000

# (tau_min, annualise_1e9, first_window) for the ONLY tenors the lane
# trades. E1 is measured at 4 h and 8 h and is gone by 12 h, and kill
# criterion 4 forbids the longer cells -- so they are absent here too,
# not merely discouraged. 15 m is BIN15's tenor: its comparator is a
# venue's binary book, not an implied vol, so kill-4 does not bind it.
TENORS = {
    TAU_15M_NS: (15, ANNUALISE_15M_1E9, 0),
    TAU_4H_NS: (240, ANNUALISE_4H_1E9, 1),
    TAU_8H_NS: (480, ANNUALISE_8H_1E9, 1),
}

I64_MAX = 2**63 - 1
I64_MIN = -(2**63)
U64_MASK = 2**64 - 1
U64_BITS = 64

# The only floats in this module: two constant tables, built from the
# same expression `crates/core-vol/src/fx.rs` builds them from and
# re-derived against it by the parity fixture.
LOG2_TAB = tuple(round(math.log2(1 + i / 256) * 1e9) for i in range(257))
EXP2_TAB = tuple(round(math.pow(2, i / 256) * (1 << 32)) for i in range(257))


def tenor_of(tau_ns):
    """``(tau_min, annualise_1e9, first_window)`` for a traded tenor.

    ``None`` for anything else. The third element is the first index of
    ``HAR_WINDOWS`` the tenor folds (BIN15 O4a); callers that predate it
    index ``[0]``/``[1]`` and are unaffected.
    """
    return TENORS.get(tau_ns)


def ret_bps_1e9(from_1e6, to_1e6):
    """Return of ``to`` over ``from`` in bps x 1e9, floored."""
    if from_1e6 <= 0:
        return 0
    return ((to_1e6 - from_1e6) * 10_000_000_000_000) // from_1e6


def isqrt_i64(v):
    """Floor square root, saturating into the i64 the Rust returns."""
    if v <= 0:
        return 0
    r = math.isqrt(v)
    return I64_MAX if r > I64_MAX else r


def log2_1e9(x):
    """``log2(x) * 1e9`` for a raw non-negative integer < 2**64."""
    if x == 0:
        return LOG2_UNDEFINED
    lz = 64 - x.bit_length()
    e = 63 - lz
    norm = (x << lz) & U64_MASK
    frac_q32 = (norm >> 31) - (1 << 32)
    idx = frac_q32 >> 24
    rem = frac_q32 & 0x00FF_FFFF
    lo = LOG2_TAB[idx]
    hi = LOG2_TAB[idx + 1]
    interp = lo + (((hi - lo) * rem) >> 24)
    return e * 1_000_000_000 + interp


def exp2_1e9(v_1e9):
    """``2 ** (v_1e9 / 1e9)`` as a raw integer, saturating at 2**64-1."""
    if v_1e9 < 0:
        return 0
    e = v_1e9 // 1_000_000_000
    if e >= U64_BITS:
        return U64_MASK
    f = v_1e9 - e * 1_000_000_000
    scaled = f * 256
    idx = scaled // 1_000_000_000
    rem = scaled - idx * 1_000_000_000
    lo = EXP2_TAB[idx]
    hi = EXP2_TAB[idx + 1]
    mant_q32 = lo + ((hi - lo) * rem) // 1_000_000_000
    out = (mant_q32 << e) >> 32
    return U64_MASK if out > U64_MASK else out


def ln_1e9(x):
    """``ln(x) * 1e9`` for a raw non-negative integer."""
    l2 = log2_1e9(x)
    if l2 == LOG2_UNDEFINED:
        return LOG2_UNDEFINED
    return (l2 * LN2_1E9) // 1_000_000_000


def exp_1e9(v_1e9):
    """``e ** (v_1e9 / 1e9)`` as a raw integer, saturating."""
    if v_1e9 < 0:
        return 0
    l2 = (v_1e9 * 1_000_000_000) // LN2_1E9
    if l2 > I64_MAX:
        return U64_MASK
    return exp2_1e9(l2)


def qlike_1e9(ln_rv_1e9, ln_sigma_1e9):
    """``u - ln u - 1`` in x1e9, where ``u = rv^2 / sigma_hat^2``."""
    ln_u = (ln_rv_1e9 - ln_sigma_1e9) * 2
    arg = (ln_u * 1_000_000_000) // LN2_1E9 + LOG2_1E9_1E9
    if arg < 0:
        u = 0
    elif arg > I64_MAX:
        return I64_MAX
    else:
        u = exp2_1e9(arg)
    u = min(u, I64_MAX)
    return u - ln_u - 1_000_000_000


def ols_fit_1e9(xs, ys):
    """``(a_1e9, b_1e9)`` -- the fit BOTH engines take (``core_vol::ols_fit_1e9``,
    lifted out of ``VolEngine::refit`` at HAR H1; the V4 fixtures are the
    proof it did not move).

    ``None`` below ``MIN_PAIRS`` pairs, on a regressor with no spread or
    less than ``X_SPREAD_MIN_1E9`` of it, or on a line outside i64. BIN15
    P1b (F5): the slope is clamped BEFORE the intercept is formed, so
    ``a`` belongs to the line the engine will use. Order-free.
    """
    n = len(xs)
    if n < MIN_PAIRS:
        return None
    xbar = sum(xs) // n
    ybar = sum(ys) // n
    sxy = 0
    sxx = 0
    for x, y in zip(xs, ys, strict=True):
        dx = x - xbar
        dy = y - ybar
        sxy += dx * dy
        sxx += dx * dx
    if sxx == 0 or sxx < n * X_SPREAD_MIN_1E9 * X_SPREAD_MIN_1E9:
        return None
    b = min(max((sxy * 1_000_000_000) // sxx, B_MIN_1E9), B_MAX_1E9)
    a = ybar - (b * xbar) // 1_000_000_000
    if not (I64_MIN <= a <= I64_MAX and I64_MIN <= b <= I64_MAX):
        return None
    return (a, b)


def trailing_means_1e9(a, b):
    """The floored means of two equally long trailing windows -- the QLIKE
    tell's arithmetic, one body for both engines (``(0, 0)`` when empty)."""
    n = len(a)
    if n == 0:
        return (0, 0)
    return (sum(a) // n, sum(b) // n)


class VolEngine:
    """The integer forecast engine, mirroring ``core_vol::VolEngine``.

    Offline code: a list is fine where the Rust uses an inline array,
    because the ring bound is what matters for parity, not the storage.
    """

    def __init__(self):
        self.sum_sq = [0] * len(HAR_WINDOWS)
        self.a_1e9 = 0
        self.b_1e9 = 0
        self.minutes = 0
        self.prev_px_1e6 = 0
        self.ret_1e9 = [0] * MINUTE_RING
        self.pair_x_1e9 = []
        self.pair_y_1e9 = []
        self.qlike_iv_1e9 = []
        self.qlike_har_1e9 = []
        self.pend_x_1e9 = 0
        self.pend_ln_sigma_1e9 = None
        self.pend_rv_iv_1e9 = 0
        # F1: the armed hold's realised window, [pend_arm_k,
        # pend_arm_k + pend_tau_min) in global minute indices.
        self.pend_arm_k = 0
        self.pend_tau_min = 0
        self.armed = False
        self.fitted = False

    # -- ingest ----------------------------------------------------

    def on_minute_close(self, px_1e6):
        """Feed one 1-minute close x1e6; rolls all four window sums."""
        if px_1e6 <= 0:
            return
        if self.prev_px_1e6 <= 0:
            self.prev_px_1e6 = px_1e6
            return
        r = ret_bps_1e9(self.prev_px_1e6, px_1e6)
        self.prev_px_1e6 = px_1e6
        k = self.minutes
        self.ret_1e9[k % MINUTE_RING] = r
        sq = r * r
        for w, win in enumerate(HAR_WINDOWS):
            self.sum_sq[w] += sq
            if k >= win:
                out = self.ret_1e9[(k - win) % MINUTE_RING]
                self.sum_sq[w] -= out * out
        self.minutes = k + 1

    # -- forecast --------------------------------------------------

    def har_1e9(self, tau_ns):
        """HAR forecast of realised vol over tau, raw bps x 1e9.

        BIN15 O4a: the fold starts at the tenor's ``first_window`` and
        divides by the number of terms actually folded -- three over
        [60, 240, 1440] for 4 h and 8 h, which is what it always was.
        """
        t = tenor_of(tau_ns)
        if t is None or self.minutes < HAR_WINDOWS[-1]:
            return None
        first = t[2]
        terms = len(HAR_WINDOWS) - first
        mean_sq = 0
        for w in range(first, len(HAR_WINDOWS)):
            rv = isqrt_i64(self.sum_sq[w])
            mean_sq += rv * rv // HAR_WINDOWS[w]
        har = isqrt_i64(mean_sq // terms * t[0])
        return har if har > 0 else None

    def x_1e9(self, tau_ns):
        """The regressor ``x = ln(har_tau)`` x1e9."""
        har = self.har_1e9(tau_ns)
        if har is None:
            return None
        x = ln_1e9(har)
        return None if x == LOG2_UNDEFINED else x

    def fit(self):
        """``(a_1e9, b_1e9)`` once at least ``MIN_PAIRS`` pairs exist."""
        return (self.a_1e9, self.b_1e9) if self.fitted else None

    def ln_sigma_hat_1e9(self, tau_ns):
        """``ln sigma_hat`` over tau, x1e9."""
        if not self.fitted:
            return None
        x = self.x_1e9(tau_ns)
        if x is None:
            return None
        return self.a_1e9 + (self.b_1e9 * x) // 1_000_000_000

    def bounds(self, tau_ns, theta_1e9):
        """``(iv_lo_1e9, iv_hi_1e9)`` as annualised fractions x1e9."""
        return self.bounds_with_offset(tau_ns, theta_1e9, 0)

    def bounds_with_offset(self, tau_ns, theta_1e9, off_1e9):
        """R3: :meth:`bounds` with an additive offset on ``ln sigma_hat``.

        The regime-edge section 3.3 fit measures what the CURRENT
        volatility word says about the next window's realised vol over
        and above what the HAR already knows, so the correction belongs
        on the forecast, in the same log-vol domain, and nowhere else.
        The target of that fit is log realised VOL, not log variance
        (``rg_lib.fwd_rv`` returns ``sqrt(sum r^2)`` and ``rg_har.build``
        takes its ``log``), so its coefficients enter here unhalved.

        ``off_1e9 == 0`` is :meth:`bounds` digit for digit, which is what
        makes an absent ``regime_*`` key inert.
        """
        t = tenor_of(tau_ns)
        if t is None:
            return None
        ln_sigma = self.ln_sigma_hat_1e9(tau_ns)
        if ln_sigma is None:
            return None
        ln_sigma += off_1e9
        lo = self._annualised_1e9(ln_sigma - theta_1e9, t[1])
        hi = self._annualised_1e9(ln_sigma + theta_1e9, t[1])
        if lo is None or hi is None:
            return None
        return (lo, hi)

    def sigma_hat_1e9(self, tau_ns):
        """BIN15 O4a: ``sigma_hat`` over tau -- ``exp(ln sigma_hat)``.

        The raw per-tau vol, no annualiser and no band: BIN15 has no
        implied vol to compare against, it needs the number itself to
        build a per-minute variance for its binary pricer. ``None`` on
        exactly the conditions that make ``ln_sigma_hat_1e9`` None, plus
        the same saturation law as ``_annualised_1e9``.
        """
        ln = self.ln_sigma_hat_1e9(tau_ns)
        if ln is None:
            return None
        sigma = exp_1e9(ln)
        if sigma in (0, U64_MASK) or sigma > I64_MAX:
            return None
        return sigma if sigma > 0 else None

    @staticmethod
    def _annualised_1e9(ln_v_1e9, annualise_1e9):
        rv = exp_1e9(ln_v_1e9)
        if rv in (0, U64_MASK):
            return None
        iv = (rv * annualise_1e9) // BPS_1E9_PER_UNIT
        if iv <= 0 or iv > I64_MAX:
            return None
        return iv

    # -- pairs -----------------------------------------------------

    def arm_hold(self, tau_ns, mark_iv_1e9):
        """Stash the regressor, the forecast and the quoted implied vol."""
        return self.arm_hold_with_offset(tau_ns, mark_iv_1e9, 0)

    def arm_hold_with_offset(self, tau_ns, mark_iv_1e9, off_1e9):
        """R3: :meth:`arm_hold`, scoring the OFFSET forecast.

        The QLIKE comparison exists to judge the forecast the member
        actually decided on (kill criterion 3). Arming with the
        uncorrected ``ln sigma_hat`` while deciding on the corrected one
        would score a forecaster nobody is running.
        """
        x = self.x_1e9(tau_ns)
        t = tenor_of(tau_ns)
        if x is None or t is None:
            return None
        # F1: the realised window opens at the NEXT minute to close, so
        # it can never contain a minute that closed before the entry.
        self.pend_arm_k = self.minutes
        self.pend_tau_min = t[0]
        self.pend_x_1e9 = x
        ln_sigma = self.ln_sigma_hat_1e9(tau_ns)
        self.pend_ln_sigma_1e9 = None if ln_sigma is None else ln_sigma + off_1e9
        if mark_iv_1e9 > 0:
            rv = (mark_iv_1e9 * BPS_1E9_PER_UNIT) // t[1]
            self.pend_rv_iv_1e9 = rv if rv <= I64_MAX else 0
        else:
            self.pend_rv_iv_1e9 = 0
        self.armed = True
        return x

    def realised_since_arm_1e9(self):
        """Realised vol of the ARMED hold, raw bps x1e9.

        ``isqrt(sum r_k^2)`` over the ``pend_tau_min`` returns pushed
        since ``arm_hold`` -- the same quantity ``vrp_seed.realised_rv_1e9``
        forms from ``candles.db``, and NOT ``har_1e9``, which is a
        forecast (F1).

        ``None`` when nothing is armed, when fewer than ``tau_min``
        returns have arrived (the hold is not over), or when the oldest
        of them has left the ring.
        """
        if not self.armed or self.pend_tau_min <= 0:
            return None
        tau = self.pend_tau_min
        elapsed = self.minutes - self.pend_arm_k
        if elapsed < tau or elapsed > MINUTE_RING:
            return None
        acc = 0
        for k in range(self.pend_arm_k, self.pend_arm_k + tau):
            r = self.ret_1e9[k % MINUTE_RING]
            acc += r * r
        rv = isqrt_i64(acc)
        return rv if rv > 0 else None

    def disarm(self):
        """F4: drop an armed hold without forming a pair -- the submit
        the arm was made for did not happen."""
        self.armed = False

    def seed_pair(self, x_1e9, y_1e9):
        """Append a pre-formed pair (the V5 boot seed) and refit."""
        self._push_pair(x_1e9, y_1e9)
        self._refit()

    def observe_settlement(self, realised_rv_1e9):
        """Settle the armed hold: form the pair, refit, score QLIKE."""
        if not self.armed or realised_rv_1e9 <= 0:
            self.armed = False
            return
        y = ln_1e9(realised_rv_1e9)
        if y == LOG2_UNDEFINED:
            self.armed = False
            return
        if self.pend_ln_sigma_1e9 is not None and self.pend_rv_iv_1e9 > 0:
            ln_iv = ln_1e9(self.pend_rv_iv_1e9)
            if ln_iv != LOG2_UNDEFINED:
                self.qlike_har_1e9.append(qlike_1e9(y, self.pend_ln_sigma_1e9))
                self.qlike_iv_1e9.append(qlike_1e9(y, ln_iv))
                if len(self.qlike_har_1e9) > QLIKE_RING:
                    self.qlike_har_1e9.pop(0)
                    self.qlike_iv_1e9.pop(0)
        self._push_pair(self.pend_x_1e9, y)
        self._refit()
        self.armed = False

    def qlike_counters(self):
        """``(n, iv_mean_1e9, har_mean_1e9, har_beats_iv)``."""
        n = len(self.qlike_har_1e9)
        if n == 0:
            return (0, 0, 0, False)
        iv, har = trailing_means_1e9(self.qlike_iv_1e9, self.qlike_har_1e9)
        return (n, iv, har, n == QLIKE_RING and har < iv)

    def _push_pair(self, x_1e9, y_1e9):
        self.pair_x_1e9.append(x_1e9)
        self.pair_y_1e9.append(y_1e9)
        if len(self.pair_x_1e9) > PAIR_RING:
            self.pair_x_1e9.pop(0)
            self.pair_y_1e9.pop(0)

    def _refit(self):
        fit = ols_fit_1e9(self.pair_x_1e9, self.pair_y_1e9)
        if fit is None:
            self.fitted = False
            return
        self.a_1e9, self.b_1e9 = fit
        self.fitted = True


# --- the long tenors (HAR H1; Rust counterpart: crates/core-vol/src/long.rs) ---
#
# One ``sum r^2`` per UTC day instead of a minute ring; the fold over
# (1, 7, 30) completed days in VolEngine's integer steps; every whole-day
# tenor 1..=40 (O-HC5); pairs made by the clock at each day close,
# overlapping, keyed by the target's first day; a rolling fit per tenor
# (``ols_fit_1e9``) and a QLIKE tell of the raw fold against the fit AS
# ARMED. The shared fixture ``tests/fixtures/vol/long-1.*`` pins this
# class to the Rust bit for bit.

DAY_MS = 86_400_000
DAY_NS = DAY_MS * 1_000_000
DAY_MINUTES = 1440
DAY_RING = 64
PAIR_RING_LONG = 128
LONG_WINDOWS_DAYS = (1, 7, 30)
LONG_WARM_DAYS = LONG_WINDOWS_DAYS[-1]
LONG_TAU_DAYS_MAX = 40
_YEAR_MIN_E18 = 525_960 * 10**18
_MINUTE_MS = 60_000
#: Restore bounds (``core_vol::long``): a log-vol within +/-100 in log, a
#: day sum within 1e32 -- the writer can never produce anything outside.
_LN_ABS_MAX_1E9 = 100_000_000_000
_SUM_SQ_MAX = 10**32
#: HAR H3.3: the weekday profile's width, and ``i128::MAX`` (its saturation).
WEEKDAYS = 7
_I128_MAX = 2**127 - 1


def annualiser_1e9(tau_min):
    """``round(sqrt(525 960 / tau_min) * 1e9)``, exactly, in integers --
    the law that reproduces ``ANNUALISE_15M/4H/8H_1E9``."""
    k = math.isqrt(_YEAR_MIN_E18 // tau_min)
    return k + 1 if 4 * _YEAR_MIN_E18 >= tau_min * (2 * k + 1) ** 2 else k


ANNUALISE_LONG_1E9 = tuple(annualiser_1e9(d * DAY_MINUTES) for d in range(1, LONG_TAU_DAYS_MAX + 1))


def long_tenor_of(tau_ns):
    """``(tau_days, tau_min, annualise_1e9)`` for a whole day ``1..=40``,
    else ``None``."""
    if tau_ns <= 0 or tau_ns % DAY_NS:
        return None
    d = tau_ns // DAY_NS
    if d > LONG_TAU_DAYS_MAX:
        return None
    return (d, d * DAY_MINUTES, ANNUALISE_LONG_1E9[d - 1])


def _tix(tau_ns):
    t = long_tenor_of(tau_ns)
    return None if t is None else t[0] - 1


def weekday_of(ts_ms):
    """The UTC weekday, Monday = 0 ... Sunday = 6 (1970-01-01 was a Thursday)."""
    return (ts_ms // DAY_MS + 3) % WEEKDAYS


class LongVolEngine:
    """The long-tenor engine, mirroring ``core_vol::LongVolEngine``.

    Offline code: the pair and QLIKE rings are chronological lists (the
    order the Rust accessors lend them in); the day ring keeps the
    Rust's slot arithmetic, because ``day_at`` and the arms depend on it.
    """

    def __init__(self):
        self.cur_sq = 0
        self.prev_px_1e6 = 0
        self.cur_day_ts_ms = 0
        self.last_min_ts_ms = 0
        self.gaps = 0
        self.refused = 0
        self.cur_n = 0
        self.dirty = False
        self.n_days = 0
        self.day_sq = [0] * DAY_RING
        self.day_ts_ms = [0] * DAY_RING
        self.day_n = [0] * DAY_RING
        self.day_x = [[LOG2_UNDEFINED] * LONG_TAU_DAYS_MAX for _ in range(DAY_RING)]
        self.day_fit = [[LOG2_UNDEFINED] * LONG_TAU_DAYS_MAX for _ in range(DAY_RING)]
        self.a_1e9 = [0] * LONG_TAU_DAYS_MAX
        self.b_1e9 = [0] * LONG_TAU_DAYS_MAX
        self.fitted = [False] * LONG_TAU_DAYS_MAX
        self.pairs = [[] for _ in range(LONG_TAU_DAYS_MAX)]
        self.qlike = [[] for _ in range(LONG_TAU_DAYS_MAX)]

    # -- the feed --------------------------------------------------

    def on_minute_close_at(self, px_1e6, min_ts_ms):
        """One 1-minute close x1e6 stamped with its minute (ms)."""
        if px_1e6 <= 0 or min_ts_ms <= self.last_min_ts_ms:
            self.refused += 1
            return
        if self.dirty:
            self.refresh()
        day_ts = min_ts_ms - min_ts_ms % DAY_MS
        if day_ts != self.cur_day_ts_ms and not self._roll_to(day_ts):
            self.refused += 1
            return
        if self.last_min_ts_ms > 0 and min_ts_ms != self.last_min_ts_ms + _MINUTE_MS:
            self.gaps += 1
        if self.prev_px_1e6 > 0:
            r = ret_bps_1e9(self.prev_px_1e6, px_1e6)
            self.cur_sq += r * r
            self.cur_n += 1
        self.prev_px_1e6 = px_1e6
        self.last_min_ts_ms = min_ts_ms

    def _last_day_ts(self):
        return self.day_ts_ms[(self.n_days - 1) % DAY_RING]

    def _roll_to(self, day_ts):
        if self.cur_day_ts_ms != 0:
            self._close_day(self.cur_day_ts_ms, self.cur_sq, self.cur_n)
        elif self.n_days > 0 and day_ts <= self._last_day_ts():
            return False
        if self.n_days > 0:
            nxt = self._last_day_ts() + DAY_MS
            if nxt < day_ts:
                # The empty-day law: no return across an unobserved day.
                self.prev_px_1e6 = 0
            if (day_ts - nxt) // DAY_MS >= DAY_RING:
                self.n_days = 0
            else:
                while nxt < day_ts:
                    self._close_day(nxt, 0, 0)
                    nxt += DAY_MS
        self.cur_day_ts_ms = day_ts
        self.cur_sq = 0
        self.cur_n = 0
        return True

    def _close_day(self, day_ts, sq, n):
        g = self.n_days
        s = g % DAY_RING
        self.day_ts_ms[s] = day_ts
        self.day_sq[s] = sq
        self.day_n[s] = n
        self.n_days = g + 1
        v = self._fold()
        for t in range(LONG_TAU_DAYS_MAX):
            tau = t + 1
            if g >= tau:
                self._settle(t, g - tau)
            x, fit = self._arm(v, t)
            self.day_x[s][t] = x
            self.day_fit[s][t] = fit

    def _settle(self, t, arm_g):
        a = arm_g % DAY_RING
        x = self.day_x[a][t]
        if x == LOG2_UNDEFINED:
            return
        first = arm_g + 1
        window = range(first, first + t + 1)
        if any(self.day_n[g % DAY_RING] == 0 for g in window):
            return
        acc = sum(self.day_sq[g % DAY_RING] for g in window)
        rv = isqrt_i64(acc)
        if rv <= 0:
            return
        y = ln_1e9(rv)
        if y == LOG2_UNDEFINED:
            return
        self._push_pair(t, self.day_ts_ms[first % DAY_RING], x, y)
        fit = self.day_fit[a][t]
        if fit != LOG2_UNDEFINED:
            self._push_qlike(t, qlike_1e9(y, x), qlike_1e9(y, fit))
        self._refit(t)

    def _arm(self, v, t):
        if v <= 0:
            return (LOG2_UNDEFINED, LOG2_UNDEFINED)
        har = isqrt_i64(v * (t + 1) * DAY_MINUTES)
        if har <= 0:
            return (LOG2_UNDEFINED, LOG2_UNDEFINED)
        x = ln_1e9(har)
        if x == LOG2_UNDEFINED:
            return (LOG2_UNDEFINED, LOG2_UNDEFINED)
        if self.fitted[t]:
            return (x, self.a_1e9[t] + (self.b_1e9[t] * x) // 1_000_000_000)
        return (x, LOG2_UNDEFINED)

    def _fold(self):
        if not self.is_warm():
            return 0
        mean_sq = 0
        acc = 0
        back = 0
        for win in LONG_WINDOWS_DAYS:
            while back < win:
                acc += self.day_sq[(self.n_days - 1 - back) % DAY_RING]
                back += 1
            rv = isqrt_i64(acc)
            mean_sq += rv * rv // (win * DAY_MINUTES)
        return mean_sq // len(LONG_WINDOWS_DAYS)

    def _push_pair(self, t, ts_ms, x, y):
        self.pairs[t].append((ts_ms, x, y))
        if len(self.pairs[t]) > PAIR_RING_LONG:
            self.pairs[t].pop(0)

    def _push_qlike(self, t, raw, fit):
        self.qlike[t].append((raw, fit))
        if len(self.qlike[t]) > QLIKE_RING:
            self.qlike[t].pop(0)

    def _refit(self, t):
        fit = ols_fit_1e9([p[1] for p in self.pairs[t]], [p[2] for p in self.pairs[t]])
        if fit is None:
            self.fitted[t] = False
            return
        self.a_1e9[t], self.b_1e9[t] = fit
        self.fitted[t] = True

    # -- state and forecasts ---------------------------------------

    def is_warm(self):
        """The newest ``LONG_WARM_DAYS`` closed days resident and every one
        OBSERVED (an empty day in the window is a hole)."""
        if self.n_days < LONG_WARM_DAYS:
            return False
        return all(
            self.day_n[(self.n_days - 1 - back) % DAY_RING] > 0 for back in range(LONG_WARM_DAYS)
        )

    def x_1e9(self, tau_ns):
        """The regressor armed at the newest close -- the RAW forecast."""
        t = _tix(tau_ns)
        if t is None or self.n_days == 0:
            return None
        x = self.day_x[(self.n_days - 1) % DAY_RING][t]
        return None if x == LOG2_UNDEFINED else x

    def fit(self, tau_ns):
        """``(a_1e9, b_1e9)`` of the tenor's rolling fit."""
        t = _tix(tau_ns)
        if t is None or not self.fitted[t]:
            return None
        return (self.a_1e9[t], self.b_1e9[t])

    def ln_sigma_fit_1e9(self, tau_ns):
        """``a + b*x/1e9`` x1e9."""
        x = self.x_1e9(tau_ns)
        f = self.fit(tau_ns)
        if x is None or f is None:
            return None
        return f[0] + (f[1] * x) // 1_000_000_000

    def sigma_ann_1e9(self, tau_ns, which):
        """A forecast (``which`` in ``("raw", "fit")``) annualised x1e9."""
        tenor = long_tenor_of(tau_ns)
        if tenor is None:
            return None
        ln = self.x_1e9(tau_ns) if which == "raw" else self.ln_sigma_fit_1e9(tau_ns)
        if ln is None:
            return None
        return VolEngine._annualised_1e9(ln, tenor[2])

    def n_pairs(self, tau_ns):
        """Pairs held for the tenor (0 off the grid)."""
        t = _tix(tau_ns)
        return 0 if t is None else len(self.pairs[t])

    def qlike_counters(self, tau_ns):
        """``(n, raw_mean_1e9, fit_mean_1e9, fit_beats_raw)``."""
        t = _tix(tau_ns)
        if t is None or not self.qlike[t]:
            return (0, 0, 0, False)
        rows = self.qlike[t]
        raw, fit = trailing_means_1e9([r[0] for r in rows], [r[1] for r in rows])
        n = len(rows)
        return (n, raw, fit, n == QLIKE_RING and fit < raw)

    # -- the weekday profile (HAR H3.3) ------------------------------

    def weekday_profile_1e6(self):
        """``([ratio_1e6] * 7, [observed_days] * 7)``, Monday first: per UTC
        weekday the mean ``sum r^2`` of its OBSERVED resident days over the
        mean of every observed day (``core_vol::LongVolEngine::
        weekday_profile_1e6``, bit for bit: floored means, the product
        saturating to ``i64::MAX`` past ``i128``, zeros without a mean)."""
        sums = [0] * WEEKDAYS
        cnt = [0] * WEEKDAYS
        total = 0
        n_obs = 0
        n = self.n_resident()
        for i in range(n):
            s = (self.n_days - n + i) % DAY_RING
            if self.day_n[s] > 0:
                w = weekday_of(self.day_ts_ms[s])
                sums[w] = min(sums[w] + self.day_sq[s], _I128_MAX)
                cnt[w] += 1
                total = min(total + self.day_sq[s], _I128_MAX)
                n_obs += 1
        out = [0] * WEEKDAYS
        if n_obs == 0:
            return out, cnt
        mean = total // n_obs
        if mean <= 0:
            return out, cnt
        for w in range(WEEKDAYS):
            if cnt[w] > 0:
                v = (sums[w] // cnt[w]) * 1_000_000
                out[w] = I64_MAX if v > _I128_MAX else min(v // mean, I64_MAX)
        return out, cnt

    # -- the writer's view -----------------------------------------

    def n_resident(self):
        """Closed days resident, at most ``DAY_RING``."""
        return min(self.n_days, DAY_RING)

    def _resident_g(self, i):
        n = self.n_resident()
        return None if i >= n else self.n_days - n + i

    def day_at(self, i):
        """``(day_ts_ms, sum_sq, n_min)`` of the i-th resident day."""
        g = self._resident_g(i)
        if g is None:
            return None
        s = g % DAY_RING
        return (self.day_ts_ms[s], self.day_sq[s], self.day_n[s])

    def open_day(self):
        """``(day_ts_ms, sum_sq, n_min)`` of the open day, or ``None``."""
        if self.cur_day_ts_ms == 0:
            return None
        return (self.cur_day_ts_ms, self.cur_sq, self.cur_n)

    def arm_at(self, i, tau_ns):
        """``(x, fit)`` armed at the close of the i-th resident day."""
        t = _tix(tau_ns)
        g = self._resident_g(i)
        if t is None or g is None:
            return None
        x = self.day_x[g % DAY_RING][t]
        if x == LOG2_UNDEFINED:
            return None
        return (x, self.day_fit[g % DAY_RING][t])

    def pair_at(self, tau_ns, i):
        """The tenor's i-th pair ``(target_ts_ms, x, y)``, oldest first."""
        t = _tix(tau_ns)
        if t is None or i >= len(self.pairs[t]):
            return None
        return self.pairs[t][i]

    def qlike_at(self, tau_ns, i):
        """The tenor's i-th QLIKE row ``(raw, fit)``, oldest first."""
        t = _tix(tau_ns)
        if t is None or i >= len(self.qlike[t]):
            return None
        return self.qlike[t][i]

    # -- restore ---------------------------------------------------

    def _contiguous(self, day_ts_ms):
        return self.n_days == 0 or day_ts_ms == self._last_day_ts() + DAY_MS

    def seed_day(self, day_ts_ms, sum_sq, n_min):
        """Restore one CLOSED day (contiguous, before any open day)."""
        if (
            self.cur_day_ts_ms != 0
            or day_ts_ms == 0
            or day_ts_ms % DAY_MS
            or not 0 <= sum_sq <= _SUM_SQ_MAX
            or not self._contiguous(day_ts_ms)
        ):
            return False
        s = self.n_days % DAY_RING
        self.day_ts_ms[s] = day_ts_ms
        self.day_sq[s] = sum_sq
        self.day_n[s] = n_min
        self.day_x[s] = [LOG2_UNDEFINED] * LONG_TAU_DAYS_MAX
        self.day_fit[s] = [LOG2_UNDEFINED] * LONG_TAU_DAYS_MAX
        self.n_days += 1
        return True

    def seed_open(self, day_ts_ms, sum_sq, n_min, last_min_ts_ms, prev_px_1e6):
        """Restore the OPEN day and the feed's position."""
        if (
            self.cur_day_ts_ms != 0
            or day_ts_ms == 0
            or day_ts_ms % DAY_MS
            or not 0 <= sum_sq <= _SUM_SQ_MAX
            or prev_px_1e6 <= 0
            or not day_ts_ms <= last_min_ts_ms < day_ts_ms + DAY_MS
            or not self._contiguous(day_ts_ms)
        ):
            return False
        self.cur_day_ts_ms = day_ts_ms
        self.cur_sq = sum_sq
        self.cur_n = n_min
        self.last_min_ts_ms = last_min_ts_ms
        self.prev_px_1e6 = prev_px_1e6
        return True

    def seed_arm(self, tau_ns, day_ts_ms, x_1e9, fit_1e9):
        """Restore a tenor's arm at the close of a resident day."""
        t = _tix(tau_ns)
        in_range = 0 <= x_1e9 <= _LN_ABS_MAX_1E9 and (
            fit_1e9 == LOG2_UNDEFINED or -_LN_ABS_MAX_1E9 <= fit_1e9 <= _LN_ABS_MAX_1E9
        )
        if t is None or not in_range:
            return False
        for i in range(self.n_resident()):
            s = self._resident_g(i) % DAY_RING
            if self.day_ts_ms[s] == day_ts_ms:
                self.day_x[s][t] = x_1e9
                self.day_fit[s][t] = fit_1e9
                return True
        return False

    def seed_pair(self, tau_ns, target_ts_ms, x_1e9, y_1e9):
        """Restore one pair, oldest first; no fit until ``refresh``."""
        t = _tix(tau_ns)
        if t is None or not (0 <= x_1e9 <= _LN_ABS_MAX_1E9 and 0 <= y_1e9 <= _LN_ABS_MAX_1E9):
            return False
        self._push_pair(t, target_ts_ms, x_1e9, y_1e9)
        self.fitted[t] = False
        self.dirty = True
        return True

    def seed_qlike(self, tau_ns, raw_1e9, fit_1e9):
        """Restore one QLIKE row, oldest first."""
        t = _tix(tau_ns)
        if t is None or LOG2_UNDEFINED in (raw_1e9, fit_1e9):
            return False
        self._push_qlike(t, raw_1e9, fit_1e9)
        return True

    def refresh(self):
        """Refit every tenor once after a restore."""
        for t in range(LONG_TAU_DAYS_MAX):
            self._refit(t)
        self.dirty = False
