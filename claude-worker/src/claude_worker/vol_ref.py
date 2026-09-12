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
    rv_W     = isqrt(sum r_k^2)                           W in {60,240,1440}
    har_tau  = isqrt((rv60^2/60 + rv240^2/240 + rv1440^2/1440) / 3 * tau_min)
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
QLIKE_RING = 60
HAR_WINDOWS = (60, 240, 1440)

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

TAU_4H_NS = 14_400_000_000_000
TAU_8H_NS = 28_800_000_000_000

# (tau_min, annualise_1e9) for the ONLY tenors the lane trades. E1 is
# measured at 4 h and 8 h and is gone by 12 h, and kill criterion 4
# forbids the longer cells -- so they are absent here too, not merely
# discouraged.
TENORS = {
    TAU_4H_NS: (240, ANNUALISE_4H_1E9),
    TAU_8H_NS: (480, ANNUALISE_8H_1E9),
}

I64_MAX = 2**63 - 1
U64_MASK = 2**64 - 1
U64_BITS = 64

# The only floats in this module: two constant tables, built from the
# same expression `crates/core-vol/src/fx.rs` builds them from and
# re-derived against it by the parity fixture.
LOG2_TAB = tuple(round(math.log2(1 + i / 256) * 1e9) for i in range(257))
EXP2_TAB = tuple(round(math.pow(2, i / 256) * (1 << 32)) for i in range(257))


def tenor_of(tau_ns):
    """``(tau_min, annualise_1e9)`` for a traded tenor, else ``None``."""
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


class VolEngine:
    """The integer forecast engine, mirroring ``core_vol::VolEngine``.

    Offline code: a list is fine where the Rust uses an inline array,
    because the ring bound is what matters for parity, not the storage.
    """

    def __init__(self):
        self.sum_sq = [0, 0, 0]
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
        """Feed one 1-minute close x1e6; rolls the three window sums."""
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
        """HAR forecast of realised vol over tau, raw bps x 1e9."""
        t = tenor_of(tau_ns)
        if t is None or self.minutes < HAR_WINDOWS[-1]:
            return None
        mean_sq = 0
        for w, win in enumerate(HAR_WINDOWS):
            rv = isqrt_i64(self.sum_sq[w])
            mean_sq += rv * rv // win
        har = isqrt_i64(mean_sq // 3 * t[0])
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
        t = tenor_of(tau_ns)
        if t is None:
            return None
        ln_sigma = self.ln_sigma_hat_1e9(tau_ns)
        if ln_sigma is None:
            return None
        lo = self._annualised_1e9(ln_sigma - theta_1e9, t[1])
        hi = self._annualised_1e9(ln_sigma + theta_1e9, t[1])
        if lo is None or hi is None:
            return None
        return (lo, hi)

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
        x = self.x_1e9(tau_ns)
        t = tenor_of(tau_ns)
        if x is None or t is None:
            return None
        # F1: the realised window opens at the NEXT minute to close, so
        # it can never contain a minute that closed before the entry.
        self.pend_arm_k = self.minutes
        self.pend_tau_min = t[0]
        self.pend_x_1e9 = x
        self.pend_ln_sigma_1e9 = self.ln_sigma_hat_1e9(tau_ns)
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
        iv = sum(self.qlike_iv_1e9) // n
        har = sum(self.qlike_har_1e9) // n
        return (n, iv, har, n == QLIKE_RING and har < iv)

    def _push_pair(self, x_1e9, y_1e9):
        self.pair_x_1e9.append(x_1e9)
        self.pair_y_1e9.append(y_1e9)
        if len(self.pair_x_1e9) > PAIR_RING:
            self.pair_x_1e9.pop(0)
            self.pair_y_1e9.pop(0)

    def _refit(self):
        n = len(self.pair_x_1e9)
        if n < MIN_PAIRS:
            self.fitted = False
            return
        xbar = sum(self.pair_x_1e9) // n
        ybar = sum(self.pair_y_1e9) // n
        sxy = 0
        sxx = 0
        for i in range(n):
            dx = self.pair_x_1e9[i] - xbar
            dy = self.pair_y_1e9[i] - ybar
            sxy += dx * dy
            sxx += dx * dx
        if sxx == 0:
            self.fitted = False
            return
        b = (sxy * 1_000_000_000) // sxx
        a = ybar - (b * xbar) // 1_000_000_000
        self.b_1e9 = b
        self.a_1e9 = a
        self.fitted = -(2**63) <= b <= I64_MAX and -(2**63) <= a <= I64_MAX
