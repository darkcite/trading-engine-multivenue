# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_ref -- the Python MIRROR of the BIN15 integer pricer (O4b).

One-for-one with ``crates/strategy-bin15/src/price.rs``: the same
lookup-table interpolation, the same second-order log-moneyness, the
same clamps, the same floors. Not an approximation of it and not a
reimplementation -- a transcription, function for function, so that the
shared fixtures ``tests/fixtures/bin15/parity-<n>.{input,expected}.tsv``
fail on ONE side if either drifts.

BIN15 S3 (ruling O-2) adds the venue's settlement law to the pricer: the
HIP-4 binary settles on the TWAP of the underlying's mark over
``[T - W, T]`` (LAW E-11), so :func:`pricing_horizon_ns` is the
variance-time of that average (``tau - 2W/3`` while the window lies
ahead, ``tau^3/(3W^2)`` inside it) and :func:`fair_value_twap` prices from
anywhere in the instance's life, the part of the average already behind
included (:func:`twap_moneyness_1e9`). :func:`binary_twap_segment` is
``core_types::binary_twap_segment``, the one arithmetic the harness
settles with and the member accumulates with. ``parity-2`` pins them.

**Tolerance is zero.** Every step is integer, so "close enough" is not a
category here. Where Rust's arithmetic is not Python's the difference is
mirrored explicitly and named:

* ``floor_div`` is ``i128::div_euclid`` with a POSITIVE divisor, which is
  Python's ``//``. Every call site in the pricer divides by a positive
  constant or by a checked-positive strike.
* ``var_1e18 = sig2.saturating_mul(tau) / 60e9`` divides with Rust's
  ``/``, which truncates toward zero. Both operands are non-negative
  there, so truncation IS floor -- but the saturation is mirrored, not
  assumed away.
* ``d_1e6 = floor_div(...) as i64`` is a TRUNCATING CAST in Rust: an
  out-of-range value wraps before the clamp sees it. Python integers do
  not wrap, so :func:`_wrap_i64` does it explicitly. The wrap is
  unreachable for any mark and strike a venue could quote (log-moneyness
  is already refused when it leaves ``i64``), but a mirror that is only
  correct on plausible inputs is not a mirror.

Why this exists at all: the fair value is what decides whether the
member crosses a book. If the engine's number and the research's number
differ by one unit in the last place, the calibration ledger the O5 desk
gate reads is measuring a different model from the one that traded, and
nothing else in the lane would notice.

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import math
import typing

#: Points in the Φ table: ``d = 0, 0.001, …, 4.096``.
PHI_POINTS: int = 4097

#: Points in each recalibration table: ``p = 0, 1/64, …, 1``.
RECAL_POINTS: int = 65

#: Φ-table step in ``d`` ×1e6.
PHI_STEP_1E6: int = 1_000

#: Widest ``|d|`` the table covers ×1e6.
D_CLAMP_1E6: int = 4_096_000

#: Recalibration bucket width in ``p`` ×1e6.
RECAL_STEP_1E6: int = 15_625

#: Widest ``|u| = |mark - K| / K`` the pricer will square, ×1e9.
#: ``price::U_CLAMP_1E9``. Beyond it the ``i128`` square overflows on
#: the Rust side and the number is not a moneyness on either.
U_CLAMP_1E9: int = 100_000_000_000

#: τ at or above which the EARLY table applies, ns.
PHASE_EARLY_NS: int = 600_000_000_000

#: τ at or above which the MID table applies, ns.
PHASE_MID_NS: int = 240_000_000_000

PHASE_EARLY: int = 0
PHASE_MID: int = 1
PHASE_LATE: int = 2
PHASES: int = 3

ONE_1E6: int = 1_000_000

I64_MAX: int = 2**63 - 1
I64_MIN: int = -(2**63)
I128_MAX: int = 2**127 - 1
I128_MIN: int = -(2**127)


def _wrap_i64(v: int) -> int:
    """Rust's ``as i64``: truncate to 64 bits, two's complement."""
    return ((v - I64_MIN) % (2**64)) + I64_MIN


def floor_div(n: int, d: int) -> int:
    """``core_regime::math::floor_div`` -- ``i128::div_euclid``.

    Only ever called with a positive ``d`` in the pricer, where
    ``div_euclid`` and Python's ``//`` agree exactly.
    """
    if d <= 0:
        raise ValueError(f"the pricer never divides by {d}")
    return n // d


def isqrt_i128(v: int) -> int:
    """``core_regime::math::isqrt_i128`` -- floor sqrt, saturating."""
    if v <= 0:
        return 0
    return min(math.isqrt(v), I64_MAX)


def phase_of(tau_ns: int) -> int:
    """Which recalibration table τ falls in."""
    if tau_ns >= PHASE_EARLY_NS:
        return PHASE_EARLY
    if tau_ns >= PHASE_MID_NS:
        return PHASE_MID
    return PHASE_LATE


@dataclasses.dataclass(slots=True)
class Bin15Luts:
    """``phi`` (4097 points) and ``recal`` (3 × 65 points), ×1e6."""

    phi: tuple[int, ...]
    recal: tuple[tuple[int, ...], ...]

    def __post_init__(self) -> None:
        if len(self.phi) != PHI_POINTS:
            raise ValueError(f"phi needs {PHI_POINTS} points, got {len(self.phi)}")
        if len(self.recal) != PHASES:
            raise ValueError(f"recal needs {PHASES} tables, got {len(self.recal)}")
        for i, t in enumerate(self.recal):
            if len(t) != RECAL_POINTS:
                raise ValueError(f"recal[{i}] needs {RECAL_POINTS} points, got {len(t)}")

    @staticmethod
    def identity() -> "Bin15Luts":
        """``Bin15Luts::identity`` -- Φ flat at 0.5, recal the identity."""
        recal: list[tuple[int, ...]] = []
        for _ in range(PHASES):
            t = [k * RECAL_STEP_1E6 for k in range(RECAL_POINTS)]
            t[RECAL_POINTS - 1] = ONE_1E6
            recal.append(tuple(t))
        return Bin15Luts(tuple(500_000 for _ in range(PHI_POINTS)), tuple(recal))

    def phi_1e6(self, d_1e6: int) -> int:
        """``Φ(|d|) ×1e6`` by linear interpolation on the 1e-3 grid.

        ``d_1e6`` must already be in ``[0, D_CLAMP_1E6]``:
        :func:`fair_value` clamps before calling and the Rust side
        ``debug_assert!``s it.
        """
        if not 0 <= d_1e6 <= D_CLAMP_1E6:
            raise ValueError(f"d_1e6 {d_1e6} is outside the table")
        idx = d_1e6 // PHI_STEP_1E6
        if idx >= PHI_POINTS - 1:
            return self.phi[PHI_POINTS - 1]
        lo = self.phi[idx]
        hi = self.phi[idx + 1]
        frac = d_1e6 - idx * PHI_STEP_1E6
        return lo + floor_div((hi - lo) * frac, PHI_STEP_1E6)

    def recal_1e6(self, phase: int, p_1e6: int) -> int:
        """``p' ×1e6`` by linear interpolation over one phase's buckets."""
        p = min(max(p_1e6, 0), ONE_1E6)
        t = self.recal[min(phase, PHASES - 1)]
        idx = p // RECAL_STEP_1E6
        if idx >= RECAL_POINTS - 1:
            return t[RECAL_POINTS - 1]
        lo = t[idx]
        hi = t[idx + 1]
        frac = p - idx * RECAL_STEP_1E6
        return lo + floor_div((hi - lo) * frac, RECAL_STEP_1E6)


def log_moneyness_1e9(mark_1e6: int, strike_1e6: int) -> int | None:
    """``ln(mark / strike) ×1e9``, to second order.

    ``u = (mark - K)/K`` and ``ln(1+u) ~ u - u^2/2``. ``None`` when
    either price is non-positive, when ``|u|`` leaves
    :data:`U_CLAMP_1E9` (the square would overflow ``i128`` in Rust, and
    a mark a hundred times its strike is a bad strike rather than a
    moneyness), or when the result leaves ``i64`` (Rust's
    ``i64::try_from(...).ok()``).
    """
    if mark_1e6 <= 0 or strike_1e6 <= 0:
        return None
    u = floor_div((mark_1e6 - strike_1e6) * 1_000_000_000, strike_1e6)
    # REFUSE BEFORE SQUARING -- the Rust side must, because ``u * u``
    # overflows ``i128`` for a strike small enough relative to the mark,
    # and Python must refuse at the same place or the two disagree on
    # exactly the inputs the guard exists for.
    if u > U_CLAMP_1E9 or u < -U_CLAMP_1E9:
        return None
    u2 = floor_div(u * u, 2_000_000_000)
    x = u - u2
    return x if I64_MIN <= x <= I64_MAX else None


@dataclasses.dataclass(slots=True, frozen=True)
class Fair:
    """``price::Fair`` -- one re-price's result."""

    p_hat_1e6: int
    p_raw_1e6: int
    d_1e6: int
    #: ``sigma * sqrt(horizon)`` x1e9 -- the denominator ``d_1e6`` was
    #: divided by (``0`` inside a settlement window with nothing left
    #: uncertain, where ``d`` is the clamp).
    den_1e9: int = 0
    #: BIN15 S3: the pricing horizon the price was computed at, ns --
    #: ``tau_ns`` for :func:`fair_value`, :func:`pricing_horizon_ns` for
    #: :func:`fair_value_twap`; it picked the recalibration phase.
    horizon_ns: int = 0


def _var_1e18(sig2_min_1e18: int, horizon_ns: int) -> int:
    """``sig2.saturating_mul(horizon) / 60e9`` -- Rust's truncating ``/``.

    Both operands are non-negative wherever the pricer calls this, so the
    truncation is a floor; the saturation is mirrored, not assumed away.
    """
    product = min(max(sig2_min_1e18 * horizon_ns, I128_MIN), I128_MAX)
    var_1e18 = abs(product) // 60_000_000_000
    return var_1e18 if product >= 0 else -var_1e18


def _fair_of_d(luts: "Bin15Luts", d_1e6: int, den_1e9: int, horizon_ns: int) -> Fair:
    """``price::fair_of_d`` -- ``d`` -> Phi -> recalibration at the horizon's phase."""
    if d_1e6 >= 0:
        p_raw_1e6 = luts.phi_1e6(d_1e6)
    else:
        p_raw_1e6 = ONE_1E6 - luts.phi_1e6(-d_1e6)
    p_hat_1e6 = luts.recal_1e6(phase_of(horizon_ns), p_raw_1e6)
    return Fair(min(max(p_hat_1e6, 0), ONE_1E6), p_raw_1e6, d_1e6, den_1e9, horizon_ns)


def fair_value(
    luts: Bin15Luts,
    mark_1e6: int,
    strike_1e6: int,
    tau_ns: int,
    sig2_min_1e18: int,
) -> Fair | None:
    """``price::fair_value``.

    ``None`` when the inputs cannot produce a number: a zero horizon, a
    non-positive variance (a cold forecast -- ABSENT DATA HOLDS), a
    non-positive price, or a variance so small its root floors to zero.
    """
    if tau_ns == 0 or sig2_min_1e18 <= 0:
        return None
    x_1e9 = log_moneyness_1e9(mark_1e6, strike_1e6)
    if x_1e9 is None:
        return None
    den_1e9 = isqrt_i128(_var_1e18(sig2_min_1e18, tau_ns))
    if den_1e9 <= 0:
        return None
    d_1e6 = _wrap_i64(floor_div(x_1e9 * 1_000_000, den_1e9))
    d_1e6 = min(max(d_1e6, -D_CLAMP_1E6), D_CLAMP_1E6)
    return _fair_of_d(luts, d_1e6, den_1e9, tau_ns)


# --- BIN15 S3: the venue's settlement, priced ----------------------

U64_MAX: int = 2**64 - 1


def _u64(v: int, what: str) -> int:
    if not 0 <= v <= U64_MAX:
        raise ValueError(f"{what} {v} is not a u64")
    return v


def _i128_ok(v: int) -> bool:
    return I128_MIN <= v <= I128_MAX


def binary_settle_open_ns(expiry_ns: int, twap_ns: int) -> int:
    """``core_types::binary_settle_open_ns`` -- the window's first instant.

    ``[expiry - twap, expiry]`` (LAW E-11), saturating at zero.
    """
    return max(_u64(expiry_ns, "expiry_ns") - _u64(twap_ns, "twap_ns"), 0)


def binary_twap_segment(
    px_1e6: int, from_ns: int, to_ns: int, open_ns: int, close_ns: int
) -> tuple[int, int]:
    """``core_types::binary_twap_segment`` -- ``(px * dt, dt)``.

    The price in force over ``[from_ns, to_ns)``, clipped to the window
    ``[open_ns, close_ns]``; ``(0, 0)`` for a piece outside it or of no
    length. The harness settles with it and the member's running average
    accumulates with it.
    """
    lo = max(from_ns, open_ns)
    hi = min(to_ns, close_ns)
    if hi <= lo:
        return (0, 0)
    dt = hi - lo
    return (px_1e6 * dt, dt)


def pricing_horizon_ns(to_expiry_ns: int, twap_ns: int) -> int:
    """``price::pricing_horizon_ns`` -- the variance-time of the settlement.

    ``twap == 0`` -> ``tau`` (settles AT ``T``); ``tau >= W`` -> ``tau -
    floor(2W/3)`` (formed without ``2W``, as Rust does); ``tau < W`` -> the
    cubic ``floor(floor(tau^2 / W) * tau / 3W)`` (``u128`` in Rust, exact
    here).
    """
    t = _u64(to_expiry_ns, "to_expiry_ns")
    w = _u64(twap_ns, "twap_ns")
    if w == 0:
        return t
    if t >= w:
        two_thirds = (w // 3) * 2 + ((w % 3) * 2) // 3
        return t - two_thirds
    return ((t * t // w) * t) // (3 * w)


def twap_moneyness_1e9(
    a_known_px_ns: int, mark_1e6: int, strike_1e6: int, to_expiry_ns: int, twap_ns: int
) -> int | None:
    """``price::twap_moneyness_1e9`` -- ``X / (S * W)`` x1e9.

    ``X = A_known + tau * S - W * K``. ``None`` on a non-positive price or
    window, on any step that would leave ``i128`` (Rust's ``checked_*``),
    or when ``|m|`` leaves :data:`U_CLAMP_1E9`.
    """
    if mark_1e6 <= 0 or strike_1e6 <= 0 or twap_ns == 0:
        return None
    if not _i128_ok(a_known_px_ns):
        return None
    s = mark_1e6
    w = _u64(twap_ns, "twap_ns")
    tau_s = _u64(to_expiry_ns, "to_expiry_ns") * s
    x = a_known_px_ns + tau_s
    w_k = w * strike_1e6
    if not (_i128_ok(tau_s) and _i128_ok(x) and _i128_ok(w_k)):
        return None
    x -= w_k
    num = x * 1_000_000_000
    den = s * w
    if not (_i128_ok(x) and _i128_ok(num) and _i128_ok(den)):
        return None
    m = floor_div(num, den)
    if m > U_CLAMP_1E9 or m < -U_CLAMP_1E9:
        return None
    return m


def fair_value_twap(
    luts: Bin15Luts,
    mark_1e6: int,
    strike_1e6: int,
    to_expiry_ns: int,
    twap_ns: int,
    a_known_px_ns: int,
    sig2_min_1e18: int,
) -> Fair | None:
    """``price::fair_value_twap`` -- the binary on the venue's settlement.

    The window ahead (``tau >= W`` or ``W == 0``): :func:`fair_value` at
    :func:`pricing_horizon_ns`. Inside it: ``d = m / (sigma *
    sqrt(horizon))`` with ``m`` the running-TWAP moneyness, the clamp on
    ``m``'s side (``>=`` settles ITM) when nothing is left uncertain.
    """
    if twap_ns == 0 or to_expiry_ns >= twap_ns:
        return fair_value(
            luts,
            mark_1e6,
            strike_1e6,
            pricing_horizon_ns(to_expiry_ns, twap_ns),
            sig2_min_1e18,
        )
    if sig2_min_1e18 <= 0:
        return None
    m_1e9 = twap_moneyness_1e9(a_known_px_ns, mark_1e6, strike_1e6, to_expiry_ns, twap_ns)
    if m_1e9 is None:
        return None
    horizon = pricing_horizon_ns(to_expiry_ns, twap_ns)
    den_1e9 = isqrt_i128(_var_1e18(sig2_min_1e18, horizon))
    if den_1e9 <= 0:
        d_1e6 = D_CLAMP_1E6 if m_1e9 >= 0 else -D_CLAMP_1E6
    else:
        d_1e6 = _wrap_i64(floor_div(m_1e9 * 1_000_000, den_1e9))
        d_1e6 = min(max(d_1e6, -D_CLAMP_1E6), D_CLAMP_1E6)
    return _fair_of_d(luts, d_1e6, den_1e9, horizon)


def floor_grid_1e6(px_1e6: int, tick_1e6: int) -> int:
    """Round ``px_1e6`` DOWN to the venue's grid (``rem_euclid``)."""
    if tick_1e6 <= 0:
        raise ValueError(f"tick must be positive, got {tick_1e6}")
    return px_1e6 - px_1e6 % tick_1e6


def ceil_grid_1e6(px_1e6: int, tick_1e6: int) -> int:
    """Round ``px_1e6`` UP to the venue's grid (``rem_euclid``)."""
    if tick_1e6 <= 0:
        raise ValueError(f"tick must be positive, got {tick_1e6}")
    r = px_1e6 % tick_1e6
    return px_1e6 if r == 0 else px_1e6 + (tick_1e6 - r)


# --- BIN15 H90: the late-window pricer (``late.rs``) ----------------

#: Points in the late Φ table: ``z = 0, 0.002, …, 8.192``.
LATE_PHI_POINTS: int = 4097
#: The late table's step in ``z`` ×1e6.
LATE_PHI_STEP_1E6: int = 2_000
#: Widest ``|z|`` the late table covers ×1e6.
LATE_Z_CLAMP_1E6: int = 8_192_000
#: One second, ns.
SEC_NS: int = 1_000_000_000
#: A grid second's mark is fresh while younger than this.
MARK_FRESH_NS: int = 5_000_000_000
#: A grid second's perp mid is fresh while younger than this.
PERP_FRESH_NS: int = 3_000_000_000
#: σ's window, grid seconds.
SIGMA_WINDOW_S: int = 900
#: The fewest valid 1-s returns σ is computed from.
SIGMA_MIN_OBS: int = 300
#: The basis median's window, grid seconds.
BASIS_WINDOW_S: int = 60
#: Samples in the 15 m settlement average.
TWAP_SAMPLES: int = 60
#: Seconds the return ring holds.
R2_RING: int = 1024
#: Seconds the short rings hold.
SHORT_RING: int = 64
#: The largest σ accepted ×1e9 per √s.
SIGMA_MAX_1E9: int = 100_000_000
_I64_MAX: int = 2**63 - 1


@dataclasses.dataclass(frozen=True)
class LateLut:
    """``late::LateLut``: ``Φ(z) ×1e6`` on ``z = 0 … 8.192``."""

    phi: tuple[int, ...]

    def __post_init__(self) -> None:
        if len(self.phi) != LATE_PHI_POINTS:
            raise ValueError(f"late phi needs {LATE_PHI_POINTS} points, got {len(self.phi)}")

    def phi_1e6(self, z_abs_1e6: int) -> int:
        idx = z_abs_1e6 // LATE_PHI_STEP_1E6
        if idx >= LATE_PHI_POINTS - 1:
            return self.phi[LATE_PHI_POINTS - 1]
        lo = self.phi[idx]
        hi = self.phi[idx + 1]
        frac = z_abs_1e6 - idx * LATE_PHI_STEP_1E6
        return lo + ((hi - lo) * frac) // LATE_PHI_STEP_1E6

    def p_of_z(self, z_1e6: int) -> int:
        z = max(-LATE_Z_CLAMP_1E6, min(LATE_Z_CLAMP_1E6, z_1e6))
        if z >= 0:
            return self.phi_1e6(z)
        return 1_000_000 - self.phi_1e6(-z)


@dataclasses.dataclass
class LateFair:
    """``late::LateFair``."""

    p_yes_1e6: int
    z_1e6: int
    sigma_1e9: int
    basis_1e9: int
    m_now_1e6: int
    n_obs: int


class LateGrid:
    """``late::LateGrid``: the 1-s grid of one underlying, transcribed."""

    def __init__(self) -> None:
        self.mark = [0] * SHORT_RING
        self.ratio = [0] * SHORT_RING
        self.r2 = [0] * R2_RING
        self.r_ok = [False] * R2_RING
        self.sum_r2 = 0
        self.n_r = 0
        self.sec = 0
        self.cur_mark_1e6 = 0
        self.cur_mark_wall = 0
        self.cur_perp_x2_1e6 = 0
        self.cur_perp_wall = 0

    def _clear_rings(self) -> None:
        self.mark = [0] * SHORT_RING
        self.ratio = [0] * SHORT_RING
        self.r2 = [0] * R2_RING
        self.r_ok = [False] * R2_RING
        self.sum_r2 = 0
        self.n_r = 0

    def _fill_second(self, s: int) -> None:
        t = s * SEC_NS
        m = (
            self.cur_mark_1e6
            if self.cur_mark_1e6 > 0
            and self.cur_mark_wall <= t
            and t - self.cur_mark_wall < MARK_FRESH_NS
            else 0
        )
        p = (
            self.cur_perp_x2_1e6
            if self.cur_perp_x2_1e6 > 0
            and self.cur_perp_wall <= t
            and t - self.cur_perp_wall < PERP_FRESH_NS
            else 0
        )
        i = s % SHORT_RING
        prev = self.mark[(s + SHORT_RING - 1) % SHORT_RING]
        self.mark[i] = m
        self.ratio[i] = (m * 2_000_000_000) // p if m > 0 and p > 0 else 0
        if s >= SIGMA_WINDOW_S:
            o = (s - SIGMA_WINDOW_S) % R2_RING
            if self.r_ok[o]:
                self.sum_r2 -= self.r2[o]
                self.n_r -= 1
                self.r_ok[o] = False
        j = s % R2_RING
        self.r_ok[j] = False
        self.r2[j] = 0
        if m > 0 and prev > 0:
            r = ((m - prev) * 1_000_000_000) // prev
            sq = min(r * r, _I64_MAX)
            self.r2[j] = sq
            self.r_ok[j] = True
            self.sum_r2 += sq
            self.n_r += 1

    def advance_to(self, target: int) -> None:
        if self.sec == 0:
            if self.cur_mark_1e6 <= 0:
                return
            self.sec = max(target - 1, 0)
        if target <= self.sec:
            return
        if target - self.sec > R2_RING:
            self._clear_rings()
            self.sec = target - 1
        s = self.sec + 1
        while s <= target:
            self._fill_second(s)
            s += 1
        self.sec = target

    def on_mark(self, wall_ns: int, mark_1e6: int) -> None:
        if mark_1e6 <= 0:
            return
        self.advance_to(max(wall_ns - 1, 0) // SEC_NS)
        self.cur_mark_1e6 = mark_1e6
        self.cur_mark_wall = wall_ns

    def on_perp(self, wall_ns: int, bid_plus_ask_1e6: int) -> None:
        if bid_plus_ask_1e6 <= 0:
            return
        self.advance_to(max(wall_ns - 1, 0) // SEC_NS)
        self.cur_perp_x2_1e6 = bid_plus_ask_1e6
        self.cur_perp_wall = wall_ns

    def sigma_1e9(self) -> int | None:
        if self.n_r < SIGMA_MIN_OBS:
            return None
        return min(isqrt_i128(self.sum_r2 // self.n_r), SIGMA_MAX_1E9)

    def basis_1e9(self, sec: int) -> int | None:
        vals: list[int] = []
        back = 0
        while back < BASIS_WINDOW_S and back <= sec:
            v = self.ratio[(sec - back) % SHORT_RING]
            if v > 0:
                vals.append(v)
            back += 1
        if not vals:
            return None
        vals.sort()
        k = len(vals)
        if k % 2 == 1:
            return vals[k // 2]
        return (vals[k // 2 - 1] + vals[k // 2]) // 2


def late_fair(  # one return per named hold, as the Rust
    grid: LateGrid,
    lut: LateLut,
    strike_1e6: int,
    expiry_ns: int,
    wall_ns: int,
    k_1e3: int,
) -> LateFair | str:
    """``late::late_fair``: the price, or the hold's name (``LateHold``'s
    Debug spelling). The grid must already be advanced to the query."""
    if strike_1e6 <= 0 or k_1e3 <= 0 or expiry_ns < TWAP_SAMPLES * SEC_NS:
        return "Shape"
    sec_d = wall_ns // SEC_NS
    last = expiry_ns // SEC_NS
    if sec_d > last or grid.sec < sec_d:
        return "Shape"
    if (
        grid.cur_mark_1e6 <= 0
        or grid.cur_mark_wall > wall_ns
        or wall_ns - grid.cur_mark_wall >= MARK_FRESH_NS
    ):
        return "MarkStale"
    if (
        grid.cur_perp_x2_1e6 <= 0
        or grid.cur_perp_wall > wall_ns
        or wall_ns - grid.cur_perp_wall >= PERP_FRESH_NS
    ):
        return "NoNowcast"
    sigma = grid.sigma_1e9()
    if sigma is None:
        return "NoSigma"
    basis = grid.basis_1e9(sec_d)
    if basis is None:
        return "NoNowcast"
    m_now = (grid.cur_perp_x2_1e6 * basis) // 2_000_000_000
    if m_now <= 0 or m_now > _I64_MAX:
        return "Shape"
    first = last + 1 - TWAP_SAMPLES
    n_obs = 0 if sec_d < first else min(sec_d - first + 1, TWAP_SAMPLES)
    obs = 0
    s = first
    while s < first + n_obs:
        v = grid.mark[s % SHORT_RING]
        if v <= 0:
            return "ObservedStale"
        obs += v
        s += 1
    n = TWAP_SAMPLES - n_obs
    first_fut = sec_d + 1 if sec_d + 1 > first else first
    a_ns = first_fut * SEC_NS - wall_ns
    var_1e9 = (a_ns * n * n + (n * (n - 1) * (2 * n - 1) // 6) * 1_000_000_000) // 3_600
    sqrt_v_1e9 = isqrt_i128(var_1e9 * 1_000_000_000)
    sd_1e6 = (k_1e3 * sigma * m_now * sqrt_v_1e9) // 1_000_000_000_000_000_000_000
    num = obs + n * m_now - TWAP_SAMPLES * strike_1e6
    if sd_1e6 > 0:
        z = (num * 1_000_000) // (TWAP_SAMPLES * sd_1e6)
        z = max(-LATE_Z_CLAMP_1E6, min(LATE_Z_CLAMP_1E6, z))
    elif num >= 0:
        z = LATE_Z_CLAMP_1E6
    else:
        z = -LATE_Z_CLAMP_1E6
    return LateFair(lut.p_of_z(z), z, sigma, basis, m_now, n_obs)


# --- the shared fixture -------------------------------------------

def _opt(v: int | None) -> str:
    return "-" if v is None else str(v)


def replay(lines: typing.Iterable[str]) -> list[str]:
    """Replay a ``parity-<n>.input.tsv`` tape, returning the rows.

    The grammar, one record per line (``#`` comments and blanks
    skipped), mirrored exactly by ``crates/strategy-bin15/tests/parity.rs``:

    ==========================================  ===========================
    ``PHI<TAB>v0<TAB>…``                        load Φ (4097 values)
    ``RECAL<TAB>phase<TAB>v0<TAB>…``            load one recal table
    ``F<TAB>mark<TAB>strike<TAB>tau<TAB>sig2``  ``F p_hat p_raw d`` or ``F - - -``
    ``X<TAB>mark<TAB>strike``                   ``X x_1e9`` or ``X -``
    ``P<TAB>d_1e6``                             ``P phi_1e6``
    ``R<TAB>phase<TAB>p_1e6``                   ``R recal_1e6``
    ``H<TAB>tau_ns``                            ``H phase``
    ``G<TAB>px_1e6<TAB>tick_1e6``               ``G floor ceil``
    ``T<TAB>to_expiry<TAB>twap``                ``T horizon`` (S3)
    ``M<TAB>a<TAB>mark<TAB>strike<TAB>tau<TAB>w``  ``M m_1e9`` or ``M -`` (S3)
    ``W<TAB>mark<TAB>strike<TAB>tau<TAB>w<TAB>a<TAB>sig2``  ``W p_hat p_raw d den horizon``
                                                  or ``W - - - - -`` (S3)
    ``S<TAB>px<TAB>from<TAB>to<TAB>open<TAB>close``  ``S area dt`` (S3)
    ``LPHI<TAB>v0<TAB>…``                       load the late Φ (H90)
    ``LG``                                      a fresh late grid (H90)
    ``LM<TAB>wall<TAB>mark``                    ``LateGrid.on_mark`` (H90)
    ``LP<TAB>wall<TAB>bid_plus_ask``            ``LateGrid.on_perp`` (H90)
    ``LQ<TAB>wall<TAB>strike<TAB>expiry<TAB>k``  ``LQ p z sigma basis m_now n_obs``
                                                 or ``LQ HOLD reason`` (H90)
    ==========================================  ===========================

    The tables travel IN the fixture rather than being rebuilt on each
    side, so what the parity pins is the SHIPPED table -- the same
    numbers ``bin15.toml.example`` carries and the engine hashes.
    """
    phi: tuple[int, ...] | None = None
    recal: list[tuple[int, ...] | None] = [None, None, None]
    luts: Bin15Luts | None = None
    late_lut: LateLut | None = None
    grid = LateGrid()
    out: list[str] = []

    def need() -> Bin15Luts:
        nonlocal luts
        if luts is None:
            if phi is None or any(t is None for t in recal):
                raise ValueError("the tape priced before loading its tables")
            luts = Bin15Luts(phi, tuple(typing.cast(list[tuple[int, ...]], recal)))
        return luts

    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        f = line.split("\t")
        tag = f[0]
        if tag == "PHI":
            phi = tuple(int(v) for v in f[1:])
            luts = None
        elif tag == "RECAL":
            recal[int(f[1])] = tuple(int(v) for v in f[2:])
            luts = None
        elif tag == "F":
            fair = fair_value(need(), int(f[1]), int(f[2]), int(f[3]), int(f[4]))
            if fair is None:
                out.append("F\t-\t-\t-")
            else:
                out.append(f"F\t{fair.p_hat_1e6}\t{fair.p_raw_1e6}\t{fair.d_1e6}")
        elif tag == "X":
            out.append(f"X\t{_opt(log_moneyness_1e9(int(f[1]), int(f[2])))}")
        elif tag == "P":
            out.append(f"P\t{need().phi_1e6(int(f[1]))}")
        elif tag == "R":
            out.append(f"R\t{need().recal_1e6(int(f[1]), int(f[2]))}")
        elif tag == "H":
            out.append(f"H\t{phase_of(int(f[1]))}")
        elif tag == "G":
            px, tick = int(f[1]), int(f[2])
            out.append(f"G\t{floor_grid_1e6(px, tick)}\t{ceil_grid_1e6(px, tick)}")
        elif tag == "T":
            out.append(f"T\t{pricing_horizon_ns(int(f[1]), int(f[2]))}")
        elif tag == "M":
            m = twap_moneyness_1e9(int(f[1]), int(f[2]), int(f[3]), int(f[4]), int(f[5]))
            out.append(f"M\t{_opt(m)}")
        elif tag == "W":
            fair = fair_value_twap(
                need(), int(f[1]), int(f[2]), int(f[3]), int(f[4]), int(f[5]), int(f[6])
            )
            if fair is None:
                out.append("W\t-\t-\t-\t-\t-")
            else:
                out.append(
                    f"W\t{fair.p_hat_1e6}\t{fair.p_raw_1e6}\t{fair.d_1e6}"
                    f"\t{fair.den_1e9}\t{fair.horizon_ns}"
                )
        elif tag == "S":
            area, dt = binary_twap_segment(int(f[1]), int(f[2]), int(f[3]), int(f[4]), int(f[5]))
            out.append(f"S\t{area}\t{dt}")
        elif tag == "LPHI":
            late_lut = LateLut(tuple(int(v) for v in f[1:]))
        elif tag == "LG":
            grid = LateGrid()
        elif tag == "LM":
            grid.on_mark(int(f[1]), int(f[2]))
        elif tag == "LP":
            grid.on_perp(int(f[1]), int(f[2]))
        elif tag == "LQ":
            if late_lut is None:
                raise ValueError("the tape priced late before loading LPHI")
            wall = int(f[1])
            grid.advance_to(wall // SEC_NS)
            lf = late_fair(grid, late_lut, int(f[2]), int(f[3]), wall, int(f[4]))
            if isinstance(lf, str):
                out.append(f"LQ\tHOLD\t{lf}")
            else:
                out.append(
                    f"LQ\t{lf.p_yes_1e6}\t{lf.z_1e6}\t{lf.sigma_1e9}\t{lf.basis_1e9}"
                    f"\t{lf.m_now_1e6}\t{lf.n_obs}"
                )
        else:
            raise ValueError(f"unknown fixture record `{tag}`")
    return out
