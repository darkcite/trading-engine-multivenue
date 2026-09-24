# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15.toml's lookup tables — the BIN15 pricer's fitted data (O4b).

``crates/strategy-bin15/src/price.rs`` is pure integer arithmetic over
three tables it never computes: ``phi_lut`` (the price map ``p(d)``:
the standard normal CDF, or a Student-t on request) and
``recal_early`` / ``recal_mid`` / ``recal_late`` (the walk-forward
recalibration of the raw lognormal price, one table per time-to-expiry
phase). The tables are DATA, so they live in the artifact the operator
re-cuts without a rebuild, and this module is what cuts them.

**Provenance — recorded deliberately, see the O4b migration entry.**
Spec §6.3 names ``bin/bt15.py``'s walk-forward fit as the source. That
one-shot is git-excluded research (the RESEARCH-IN-GIT law) and is not
in this tree, so it cannot be imported or re-run here. It does not need
to be: the spec states every number the tables require.

* Φ is *mathematics*, not a fit. It is computed here exactly, by the
  Maclaurin series for ``erf`` evaluated in :mod:`decimal` at 60
  significant digits and rounded half-away-from-zero to 1e-6. No float
  touches it, so the table is bit-identical on every host — which
  matters, because the engine hashes these BYTES and the boot tell names
  the hash.
* **A Student-t map (DISTX, 2026-09-23)** replaces Φ on request:
  ``--phi student-t --phi-nu NU --phi-s S`` cuts ``p(d) = T_nu(k d)``
  with ``k = s sqrt(nu / (nu - 2))`` — unit variance at ``s = 1``, so
  ``s`` is the map's scale against Φ's. ``T_nu`` is computed in
  :mod:`decimal` too (the regularised incomplete beta by Lentz's
  continued fraction, ``ln Gamma`` by Stirling's series with exact
  Bernoulli fractions), so its table is as bit-identical as Φ's. ``nu``
  and ``s`` ARE a fit: they arrive as ARGUMENTS (decimal strings, never
  floats) and no fitted value is a default in this module — research
  never enters git. A fat-tailed map ends well short of Φ's 999_979 at
  the clamp; the grammar's floor is :data:`PHI_LAST_MIN_1E6`.
* The recalibration is the fitted part, and the fit is three numbers:
  the slopes (early / mid / late). The table is that slope through the
  middle of the unit interval, with the ENDPOINTS pinned and every
  interior bucket held one venue tick inside them. A slope over 1
  pushes confidence OUTWARD, which is what an under-confident model
  needs — up to the point where the outward push runs off the end of
  the unit interval, which :func:`recal_table_1e6` refuses rather than
  clamps (BIN15 P1a / F1).
* **The 1.104 / 1.165 / 1.219 slopes are WITHDRAWN (2026-09-13).** They
  were measured on ~33 instances and they manufactured certainty in the
  tails; the shipped slopes are 1.000 (identity) until an empirical
  re-fit off the accumulated calibration ledger exists. See
  :data:`SLOPE_EARLY_MILLI`.
* ``scale_1e9`` is the stated variance-ratio scale, **0.98**.
* ``hour_ln_off_1e9`` is OMITTED unless ``--hour-ln-off-1e9`` gives
  one. The parser's law is "absent optional = bit-identical to the
  stated default", and the stated default is zero, so an artifact that
  carries 24 zeros and one that carries nothing are the same member —
  but only the second one is honest about not having an hour-of-day
  fit. A table that IS given is a fit like ``nu`` and ``s``: 24
  integers, one per UTC hour, each an offset on ``ln sigma-hat`` x1e9
  within ln 2 (:data:`HOUR_LN_OFF_ABS_MAX_1E9`), rendered on the line
  after ``scale_1e9``.

Re-fitting later is an artifact edit and a restart, never a code
change: pass ``--slope-early`` / ``--slope-mid`` / ``--slope-late``
(milli-units, so 1020 is 1.020; the bound is 1032), ``--scale-1e9``,
``--phi student-t --phi-nu NU --phi-s S`` and
``--hour-ln-off-1e9 H0,...,H23`` — to either lane.

**The one-line law.** ``core_config::icdp::parse_value`` — which
``core_config::bin15`` reuses — reads an array by stripping ``[`` and
``]`` off ONE trimmed line. A pretty-printed multi-line TOML array is
not a syntax this grammar has; it is an unterminated array. So every
table is emitted as one line, and ``phi_lut``'s is about 33 KB long.
That is not an oversight to tidy up later.

Lanes (``python -m claude_worker.bin15_fit <lane>``):

- ``tables`` [``--out <path>``] — just the generated key lines, for
  splicing into an operator file that already exists.
- ``artifact --out <path>`` [``--families ...``] [``--underlying ...``]
  — a complete ``bin15.toml``: the operator knobs at the spec's stated
  values plus the generated tables. This is how ``bin15.toml.example``
  is produced, and the file it writes parses.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import decimal
import fractions
import functools
import math
import pathlib
import sys
import typing

#: Points in ``phi_lut``: ``d = 0, 0.001, …, 4.096``. Mirrors
#: ``core_config::bin15::PHI_POINTS`` and
#: ``strategy_bin15::price::PHI_POINTS``.
PHI_POINTS: int = 4097

#: Points in each recalibration table: ``p = 0, 1/64, …, 1``.
RECAL_POINTS: int = 65

#: Φ-table step in ``d`` ×1e6 — the table is on a 1e-3 grid.
PHI_STEP_1E6: int = 1_000

#: Recalibration bucket width in ``p`` ×1e6: ``1e6 / 64``.
RECAL_STEP_1E6: int = 15_625

#: Recalibration slopes in MILLI-units, per phase. 1000 is 1.000 =
#: IDENTITY. Integers because the artifact is integer-only and a slope
#: spelled as a float would be rounded somewhere invisible.
#:
#: **BIN15 P1a (F1), 2026-09-13 — the fitted 1104 / 1165 / 1219 are
#: WITHDRAWN.** They came from ~33 instances and they are a MID-RANGE
#: correction extrapolated into the tails, where the raw lognormal
#: price is over-confident rather than under-confident. Their arithmetic
#: consequence is the reason they are gone: at a slope of 1.104 the
#: bucket at p = 63/64 recalibrates to 1.0349, which :func:`recal_table_1e6`
#: then clamps to exactly 1e6 — the member publishing CERTAINTY, and the
#: entry rule of §0 (``p > a``) reading a manufactured 1.0 as an
#: unbeatable edge over any ask. Identity plus the P6 evaluation is
#: honest; amplified certainty is not. A re-fit ships new numbers here
#: and re-cuts the artifact; it is never a code change.
SLOPE_EARLY_MILLI: int = 1_000
SLOPE_MID_MILLI: int = 1_000
SLOPE_LATE_MILLI: int = 1_000

#: BIN15 P1a: the recalibration grid tick x1e6 — one HIP-4 price tick
#: (1e-4). Mirrors ``strategy_bin15::GRID_TICK_1E6``. Interior buckets
#: are clamped one tick inside [0, 1e6] so no fitted slope can publish a
#: probability the venue cannot even quote.
GRID_TICK_1E6: int = 100

#: The stated variance-ratio scale on σ̂, ×1e9 (0.98).
SCALE_1E9_DEFAULT: int = 980_000_000

#: Working precision for Φ. The alternating ``erf`` series loses about
#: three digits to cancellation at the table's widest ``d``; 60 leaves
#: fifty to spare against a 1e-6 output.
PRECISION: int = 60

#: π to 60 significant digits. A mathematical constant, written out
#: rather than computed so the table has no iteration to trust.
PI_STR: str = "3.14159265358979323846264338327950288419716939937510582097494"

#: Terms of the ``erf`` series to allow before giving up. The series
#: converges by ~60 terms at the widest ``d``; the cap only exists so a
#: mis-edit cannot spin.
_MAX_TERMS: int = 400

#: The smallest ``phi_lut[4096]`` the grammar admits, x1e6: the price
#: map's value at the clamp ``|d| = 4.096``. Mirrors
#: ``core_config::bin15::PHI_LAST_MIN_1E6`` — a test reads the Rust
#: source to keep the two equal. The floor refuses a table that is not a
#: CDF running out to near-certainty (a truncated cut, a slipped scale);
#: it is not a statement about the tail.
PHI_LAST_MIN_1E6: int = 980_000

#: ``phi_lut[0]``: every map is symmetric about ``d = 0``, so the grammar
#: pins its first point at exactly one half, x1e6.
_PHI_FIRST_1E6: int = 500_000

#: One, x1e6 — the ceiling of every probability table.
_UNIT_1E6: int = 1_000_000

#: The ``--phi`` kinds. ``normal`` is Φ and the default.
PHI_NORMAL: str = "normal"
PHI_STUDENT_T: str = "student-t"
PHI_KINDS: tuple[str, ...] = (PHI_NORMAL, PHI_STUDENT_T)

#: A t's variance exists only above two degrees of freedom, and ``k``
#: normalises by it, so a table needs ``nu`` over this.
_T_NU_MIN: int = 2

#: ``ln Gamma`` runs Stirling's series at arguments of at least this; a
#: smaller one is shifted up by ``Gamma(z + 1) = z Gamma(z)`` first. At 50
#: the first omitted term of the 30-term series is under 1e-70.
_STIRLING_MIN: int = 50

#: Terms of Stirling's series: ``B_2`` through ``B_60``.
_STIRLING_TERMS: int = 30

#: Iterations of the incomplete-beta continued fraction to allow. It
#: needs about 50 at the DISTX shape and about 600 at ``nu = 1e7``; the
#: cap only exists so a mis-edit cannot spin.
_MAX_CF_ITERS: int = 50_000

#: The continued fraction's convergence bar, and the floor Lentz's method
#: lifts a vanishing denominator to.
_CF_EPS: decimal.Decimal = decimal.Decimal(10) ** -(PRECISION - 10)
_CF_TINY: decimal.Decimal = decimal.Decimal(10) ** -(4 * PRECISION)

#: Hours in ``hour_ln_off_1e9``. Mirrors ``core_config::bin15::HOURS``.
HOURS: int = 24

#: The fitter's own bound on one hour offset, x1e9: ln 2. An offset past
#: it halves or doubles sigma-hat for its hour, which is a different
#: forecast, not an hour-of-day correction to this one. (The grammar
#: reads any 24 integers; this guard is the fitter's.)
HOUR_LN_OFF_ABS_MAX_1E9: int = 693_147_181

#: A fit parameter: a decimal string, an exact ``Decimal`` or an int —
#: never a float, which has already rounded the number the fit made.
DecimalLike: typing.TypeAlias = decimal.Decimal | str | int

#: The spec's stated operator knobs (§6.3). Order is the order the
#: artifact is written in, which is the order the spec lists them.
KNOBS: tuple[tuple[str, int], ...] = (
    ("tau_ns", 900_000_000_000),
    ("e_take_1e6", 30_000),
    ("h_quote_1e6", 25_000),
    ("tau_min_take_ns", 60_000_000_000),
    ("tau_min_quote_ns", 120_000_000_000),
    ("tail_refuse_ns", 10_000_000_000),
    ("requote_ttl_ns", 1_000_000_000),
    ("requote_thr_1e6", 5_000),
    ("clip_qty_1e6", 500_000_000),
    ("cap_instance_usd_1e6", 1_000_000_000),
    # OPERATOR RULING 2026-09-13: $30,000/day.
    #
    # The entry law alone wants $19,200 (4 live 15 m families x 96
    # instances x $50), so $30,000 leaves **$10,800 of HEADROOM** — and
    # the headroom is the point. Arm B's resting BIDS book day room too,
    # and a maker quote that FILLS is exposure that is never released,
    # so a cap set to exactly the entry want would have had the two arms
    # competing for the last dollar late in a full day. At the maker's
    # 500-contract clip near $0.50 (~$250 a quote) $10,800 is ~43 filled
    # maker bids a day across every family, against a venue thin enough
    # to print single-digit trades per instance.
    #
    # The superseded numbers, for the record: $5,000 (the O9 default,
    # which rationed the day after its first quarter and censored the
    # calibration ledger — F2) and $19,200 (the exact-fit ruling of the
    # same afternoon).
    ("cap_day_usd_1e6", 30_000_000_000),
    # BIN15 P0 (F4): the underlying mark's shelf life, ns. 5 s is two
    # to five missed Hyperliquid mark prints. Past this age the member
    # HOLDS instead of pricing four families off a frozen number while
    # their books track reality.
    ("mark_stale_ns", 5_000_000_000),
    # BIN15 O9 (operator ruling 2026-09-13): the coverage-entry
    # notional, x1e6 USD. 50000000 = $50 on EVERY 15 m instance,
    # regardless of edge. 0 would be the pre-2026-09-13 edge law.
    ("entry_usd_1e6", 50_000_000),
    # BIN15 P3 (F6): the coverage entry's margin over its own belief,
    # x1e6. 20000 = 2 c. The entry fires only when `ask <= p_hat -
    # e_entry`, which is the `p > a` profitability bar plus model error
    # and the exit-leg fee. Distinct from `e_take_1e6` (3 c), which is
    # the opportunistic taker's edge hunt.
    ("e_entry_1e6", 20_000),
    # BIN15 R0 (2026-09-19): the coverage entry's price FLOOR, x1e6.
    # 0 = no floor = the 2026-09-13 law bit for bit. A preferred-side
    # ask far under the belief is the venue disagreeing with the
    # model, and on the paper tape the venue won those; the testnet
    # research artifact sets 500000 (0.50) or 700000 (0.70) by hand.
    ("entry_min_px_1e6", 0),
    # BIN15 S5 (2026-09-24): the coverage entry's PERSISTENCE -- how many
    # consecutive distinct book snapshots of the preferred leg its price
    # test must hold on before it fires, in [1, 8]. 1 = the first passing
    # snapshot fires, the pre-S5 law bit for bit. A persistence rule is a
    # hand edit of the live artifact, never a fitted default.
    ("entry_persist_polls", 1),
    # BIN15 S5: the entry's ELAPSED CEILING, ns after the instance's start
    # (expiry - 900 s): a run of passing snapshots must BEGIN by then.
    # 0 = no ceiling.
    ("entry_elapsed_max_ns", 0),
    ("maker_enabled", 1),
    ("null_arm", 1),
)

#: Ruling O-Q7's eight families, in the order ``universe.toml``'s
#: ``[hyperliquid] rolling`` must list them. ``bin15_boot`` compares the
#: two lists AS SEQUENCES and refuses a permutation.
FAMILIES_DEFAULT: tuple[str, ...] = (
    "out:BTC:15m",
    "out:ETH:15m",
    "out:SOL:15m",
    "out:HYPE:15m",
    "native:BTC:1d",
    "native:ETH:1d",
    "native:SOL:1d",
    "native:HYPE:1d",
)

#: The mark source per distinct underlying.
UNDERLYING_DEFAULT: tuple[str, ...] = (
    "hyperliquid:BTC",
    "hyperliquid:ETH",
    "hyperliquid:SOL",
    "hyperliquid:HYPE",
)


def round_half_away(num: int, den: int) -> int:
    """``num / den`` rounded half AWAY from zero, in integers.

    Away from zero rather than up, because it is the only rounding that
    keeps the recalibration table antisymmetric about ``(0.5, 0.5)``:
    ``t[k] + t[64 - k] == 1e6`` for every ``k``. Half-up would break
    that on the eight buckets where the slope lands exactly on a half
    unit, and a recalibration that is not symmetric prices Yes and No
    to something other than 1.
    """
    if den <= 0:
        raise ValueError(f"den must be positive, got {den}")
    if num >= 0:
        return (2 * num + den) // (2 * den)
    return -((-2 * num + den) // (2 * den))


def _erf(x: decimal.Decimal) -> decimal.Decimal:
    """``erf(x)`` for ``x >= 0`` by its Maclaurin series.

    ``erf(x) = 2/sqrt(pi) * sum_n (-1)^n x^(2n+1) / (n! (2n+1))``. The
    series is alternating and its terms peak near ``n = x^2`` before
    decaying, so it is evaluated at :data:`PRECISION` digits and
    summed until a term cannot move the result.
    """
    if x < 0:
        raise ValueError("erf here is the non-negative half only")
    pi = decimal.Decimal(PI_STR)
    total = decimal.Decimal(0)
    term = x  # n = 0: x^1 / (0! * 1)
    n = 0
    x2 = x * x
    while n < _MAX_TERMS:
        contrib = term / (2 * n + 1)
        if n % 2 == 0:
            total += contrib
        else:
            total -= contrib
        # The next term's numerator: x^(2n+3) / (n+1)!
        n += 1
        term = term * x2 / n
        if term / (2 * n + 1) < decimal.Decimal(10) ** -(PRECISION - 10):
            # One more, so the truncation is never on the side that
            # biases the sum.
            contrib = term / (2 * n + 1)
            total += -contrib if n % 2 else contrib
            break
    else:  # pragma: no cover - the cap is a guard, not a path
        raise ArithmeticError(f"erf({x}) did not converge in {_MAX_TERMS} terms")
    return 2 * total / pi.sqrt()


def phi_table_1e6() -> tuple[int, ...]:
    """``Φ(d) ×1e6`` for ``d = 0, 0.001, …, 4.096``.

    Monotone non-decreasing by construction (Φ is strictly increasing
    and the rounding is monotone), ``[0] == 500_000`` exactly, and
    ``[4096] == 999_979`` — comfortably over the grammar's floor,
    :data:`PHI_LAST_MIN_1E6`.
    """
    with decimal.localcontext() as ctx:
        ctx.prec = PRECISION
        root2 = decimal.Decimal(2).sqrt()
        half = decimal.Decimal(1) / 2
        out: list[int] = []
        i = 0
        while i < PHI_POINTS:
            d = decimal.Decimal(i * PHI_STEP_1E6) / 1_000_000
            phi = (1 + _erf(d / root2)) * half
            scaled = phi * 1_000_000
            # Exact integer rounding on the Decimal's own digits: take
            # the integral part and the remainder rather than trusting a
            # context rounding mode set somewhere else.
            whole = int(scaled)
            frac = scaled - whole
            out.append(whole + 1 if frac >= half else whole)
            i += 1
    return tuple(out)


@functools.lru_cache(maxsize=1)
def _bernoulli_even() -> tuple[fractions.Fraction, ...]:
    """``B_2, B_4, ..., B_60`` as exact fractions, for Stirling's series.

    From the defining recurrence ``sum_{j=0..m} C(m+1, j) B_j = 0``, in
    :class:`fractions.Fraction`, so not one digit of a coefficient is
    transcribed by hand.
    """
    top = 2 * _STIRLING_TERMS
    b: list[fractions.Fraction] = [fractions.Fraction(1)]
    m = 1
    while m <= top:
        acc = fractions.Fraction(0)
        j = 0
        while j < m:
            acc += math.comb(m + 1, j) * b[j]
            j += 1
        b.append(-acc / (m + 1))
        m += 1
    return tuple(b[2 * k] for k in range(1, _STIRLING_TERMS + 1))


def _ln_gamma(z: decimal.Decimal) -> decimal.Decimal:
    """``ln Gamma(z)`` for ``z > 0``, in the CALLER's :mod:`decimal` context.

    Stirling's series ``(w - 1/2) ln w - w + ln(2 pi) / 2 + sum_k B_2k /
    (2k (2k - 1) w^(2k-1))`` at ``w = z + n >=`` :data:`_STIRLING_MIN`,
    brought back down the recurrence: ``ln Gamma(z) = ln Gamma(w) -
    ln(z (z + 1) ... (w - 1))``.
    """
    if z <= 0:
        raise ValueError(f"ln_gamma here is for z > 0, got {z}")
    w = z
    shift = decimal.Decimal(1)
    while w < _STIRLING_MIN:
        shift *= w
        w += 1
    half = decimal.Decimal(1) / 2
    total = (w - half) * w.ln() - w + half * (2 * decimal.Decimal(PI_STR)).ln()
    w2 = w * w
    power = w
    coeffs = _bernoulli_even()
    k = 1
    while k <= _STIRLING_TERMS:
        b = coeffs[k - 1]
        total += decimal.Decimal(b.numerator) / (
            decimal.Decimal(b.denominator) * (2 * k) * (2 * k - 1) * power
        )
        power *= w2
        k += 1
    return total - shift.ln()


def _beta_cf(a: decimal.Decimal, b: decimal.Decimal, x: decimal.Decimal) -> decimal.Decimal:
    """The continued fraction of ``I_x(a, b)``, by the modified Lentz method.

    It converges fast for ``x < (a + 1) / (a + b + 2)``, and
    :func:`_inc_beta` swaps to the other side past that, so this never
    sees a slow ``x``.
    """
    one = decimal.Decimal(1)
    qab = a + b
    qap = a + one
    qam = a - one
    c = one
    d = one - qab * x / qap
    if abs(d) < _CF_TINY:
        d = _CF_TINY
    d = one / d
    h = d
    m = 1
    while m <= _MAX_CF_ITERS:
        m2 = 2 * m
        # The even step of the fraction ...
        aa = m * (b - m) * x / ((qam + m2) * (a + m2))
        d = one + aa * d
        if abs(d) < _CF_TINY:
            d = _CF_TINY
        c = one + aa / c
        if abs(c) < _CF_TINY:
            c = _CF_TINY
        d = one / d
        h *= d * c
        # ... and the odd one.
        aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2))
        d = one + aa * d
        if abs(d) < _CF_TINY:
            d = _CF_TINY
        c = one + aa / c
        if abs(c) < _CF_TINY:
            c = _CF_TINY
        d = one / d
        delta = d * c
        h *= delta
        if abs(delta - one) < _CF_EPS:
            return h
        m += 1
    raise ArithmeticError(  # pragma: no cover - the cap is a guard, not a path
        f"I_x(a, b) did not converge in {_MAX_CF_ITERS} iterations (a={a} b={b} x={x})"
    )


def _inc_beta(
    a: decimal.Decimal,
    b: decimal.Decimal,
    x: decimal.Decimal,
    y: decimal.Decimal,
    ln_beta: decimal.Decimal,
) -> decimal.Decimal:
    """The regularised incomplete beta ``I_x(a, b)``.

    ``y = 1 - x`` arrives EXACT rather than recomputed — near ``x = 1`` it
    is the small one, and ``1 - x`` would cancel its digits away — and
    ``ln_beta = ln B(a, b)`` arrives precomputed. Past the continued
    fraction's fast region the symmetry ``I_x(a, b) = 1 - I_y(b, a)``
    takes over.
    """
    if x <= 0:
        return decimal.Decimal(0)
    if y <= 0:
        return decimal.Decimal(1)
    front = (a * x.ln() + b * y.ln() - ln_beta).exp()
    if x < (a + 1) / (a + b + 2):
        return front * _beta_cf(a, b, x) / a
    return 1 - front * _beta_cf(b, a, y) / b


@functools.lru_cache(maxsize=8)
def _ln_beta_half(nu: decimal.Decimal) -> decimal.Decimal:
    """``ln B(nu / 2, 1 / 2)`` at :data:`PRECISION`.

    Every point of one table shares it, so it is computed once per
    ``nu`` — in its own context, so the cache can never hand back a value
    cut at a different precision.
    """
    with decimal.localcontext() as ctx:
        ctx.prec = PRECISION
        ctx.rounding = decimal.ROUND_HALF_EVEN
        half = decimal.Decimal(1) / 2
        a = nu / 2
        return _ln_gamma(a) + _ln_gamma(half) - _ln_gamma(a + half)


def _t_cdf(nu: decimal.Decimal, x: decimal.Decimal, ln_beta: decimal.Decimal) -> decimal.Decimal:
    """``T_nu(x) = 1 - I_z(nu / 2, 1 / 2) / 2`` with ``z = nu / (nu + x^2)``, ``x >= 0``."""
    half = decimal.Decimal(1) / 2
    if x == 0:
        return half
    x2 = x * x
    den = nu + x2
    return 1 - _inc_beta(nu / 2, half, nu / den, x2 / den, ln_beta) * half


def _decimal_param(name: str, value: DecimalLike) -> decimal.Decimal:
    """A fit parameter as an exact :class:`decimal.Decimal`.

    A float is REFUSED rather than converted: it has already rounded the
    number the fit produced, and a table must not depend on which binary
    neighbour that rounding picked.
    """
    if isinstance(value, bool) or not isinstance(value, decimal.Decimal | str | int):
        raise TypeError(
            f"{name} must be a decimal string, a decimal.Decimal or an int, "
            f"never {type(value).__name__}"
        )
    try:
        out = decimal.Decimal(value)
    except decimal.InvalidOperation as exc:
        raise ValueError(f"{name} is not a decimal number: {value!r}") from exc
    if not out.is_finite():
        raise ValueError(f"{name} must be finite, got {out}")
    return out


def student_t_cdf(nu: DecimalLike, x: DecimalLike) -> decimal.Decimal:
    """Student's t CDF ``T_nu(x)``, for ``x >= 0`` and any ``nu > 0``.

    In :mod:`decimal` at :data:`PRECISION`, like Φ. Exposed so the tests
    can hold the function itself to closed forms — ``T_2(x) = 1/2 +
    x / (2 sqrt(2 + x^2))`` — at a ``nu`` the table builder refuses.
    """
    nu_d = _decimal_param("nu", nu)
    x_d = _decimal_param("x", x)
    if nu_d <= 0:
        raise ValueError(f"nu must be positive, got {nu_d}")
    if x_d < 0:
        raise ValueError("T_nu here is the non-negative half only")
    with decimal.localcontext() as ctx:
        ctx.prec = PRECISION
        ctx.rounding = decimal.ROUND_HALF_EVEN
        return _t_cdf(nu_d, x_d, _ln_beta_half(nu_d))


def validate_phi_lut(table: typing.Sequence[int], what: str = "phi_lut") -> None:
    """Refuse, HERE, a ``phi_lut`` the grammar would refuse at boot.

    ``core_config::bin15`` wants exactly :data:`PHI_POINTS` values in
    ``0..=1_000_000``, ``[0] == 500_000``, a monotone table (a dip makes
    the fair value non-monotone in the mark) and ``[4096] >=``
    :data:`PHI_LAST_MIN_1E6`. An artifact that fails any of them is a
    boot refusal in the engine's KeepAlive loop; the fitter says so first.
    """
    n = len(table)
    if n != PHI_POINTS:
        raise ValueError(f"{what}: {n} points, the grammar wants {PHI_POINTS}")
    if table[0] != _PHI_FIRST_1E6:
        raise ValueError(f"{what}: [0] = {table[0]}, the grammar wants {_PHI_FIRST_1E6}")
    i = 0
    while i < n:
        v = table[i]
        if not 0 <= v <= _UNIT_1E6:
            raise ValueError(f"{what}: [{i}] = {v} is outside 0..={_UNIT_1E6}")
        if i > 0 and v < table[i - 1]:
            raise ValueError(f"{what}: not monotone at [{i}]: {table[i - 1]} then {v}")
        i += 1
    if table[-1] < PHI_LAST_MIN_1E6:
        raise ValueError(
            f"{what}: [{n - 1}] = {table[-1]} is under the grammar's floor "
            f"{PHI_LAST_MIN_1E6} (core_config::bin15::PHI_LAST_MIN_1E6) - the "
            "engine would refuse this artifact at boot"
        )


def student_t_table_1e6(nu: DecimalLike, s: DecimalLike) -> tuple[int, ...]:
    """``T_nu(k d) x1e6`` for ``d = 0, 0.001, ..., 4.096``, ``k = s sqrt(nu / (nu - 2))``.

    Rounded EXACTLY as :func:`phi_table_1e6` rounds: the integral part,
    and one more when the remainder is at least one half. Refused: ``nu``
    at or under 2 (``k`` normalises by the variance, which exists only
    above it), ``s`` at or under 0, a float for either, and a table the
    grammar would refuse (:func:`validate_phi_lut`).
    """
    nu_d = _decimal_param("nu", nu)
    s_d = _decimal_param("s", s)
    if nu_d <= _T_NU_MIN:
        raise ValueError(
            f"nu must be over {_T_NU_MIN}, got {nu_d}: k = s sqrt(nu/(nu-2)) needs "
            "the t's variance, which exists only above two degrees of freedom"
        )
    if s_d <= 0:
        raise ValueError(f"s must be positive, got {s_d}")
    return _student_t_table(nu_d, s_d)


@functools.lru_cache(maxsize=8)
def _student_t_table(nu: decimal.Decimal, s: decimal.Decimal) -> tuple[int, ...]:
    """The table itself — cached, because a CLI run renders it and then
    reports its endpoints."""
    with decimal.localcontext() as ctx:
        ctx.prec = PRECISION
        ctx.rounding = decimal.ROUND_HALF_EVEN
        half = decimal.Decimal(1) / 2
        ln_beta = _ln_beta_half(nu)
        k = s * (nu / (nu - 2)).sqrt()
        out: list[int] = []
        i = 0
        while i < PHI_POINTS:
            d = decimal.Decimal(i * PHI_STEP_1E6) / 1_000_000
            scaled = _t_cdf(nu, k * d, ln_beta) * 1_000_000
            whole = int(scaled)
            frac = scaled - whole
            out.append(whole + 1 if frac >= half else whole)
            i += 1
    table = tuple(out)
    validate_phi_lut(table, f"student-t nu={nu} s={s}")
    return table


def recal_table_1e6(slope_milli: int) -> tuple[int, ...]:
    """``p' ×1e6`` at ``p = 0, 1/64, …, 1`` for one recalibration slope.

    ``p' = 0.5 + slope * (p - 0.5)``, clamped — and **BIN15 P1a (F1)
    changed what the clamp is**. Only the two ENDPOINTS may be certain:
    ``[0] == 0`` and ``[64] == 1e6`` are the buckets where the raw price
    itself said 0 or 1. Every interior bucket is clamped to
    ``[GRID_TICK_1E6, 1e6 - GRID_TICK_1E6]``, one venue tick inside the
    unit interval.

    The old law clamped the whole table at ``[0, 1e6]``, so a slope over
    ~1.032 turned the two buckets nearest the ends into flat certainty:
    the member published ``p_hat = 1.000000`` off a raw price of 0.984,
    and a probability of exactly one beats every ask there is. A
    recalibration may sharpen a belief; it may not manufacture one the
    model never held.

    A slope whose UNCLAMPED value at bucket 63 would exceed 1e6 is
    REFUSED rather than clamped, because clamping it is precisely the
    silent failure above — the caller asked for a table this function
    cannot honestly produce. The bound is 1032 milli-units
    (``0.5 + slope * (63/64 - 0.5) <= 1``), which is why the withdrawn
    1104 / 1165 / 1219 raise here.
    """
    if slope_milli <= 0:
        raise ValueError(f"slope_milli must be positive, got {slope_milli}")
    top = 500_000 + round_half_away(
        (63 * RECAL_STEP_1E6 - 500_000) * slope_milli, 1_000
    )
    if top > 1_000_000:
        raise ValueError(
            f"slope_milli {slope_milli} recalibrates bucket 63 to {top} > 1000000: "
            "the table would pin interior certainty, which is a belief the model "
            "never held. Re-fit the slope (bound 1032) or ship identity (1000)."
        )
    out: list[int] = []
    k = 0
    while k < RECAL_POINTS:
        p = k * RECAL_STEP_1E6
        v = 500_000 + round_half_away((p - 500_000) * slope_milli, 1_000)
        if k == 0:
            v = 0
        elif k == RECAL_POINTS - 1:
            v = 1_000_000
        else:
            v = min(max(v, GRID_TICK_1E6), 1_000_000 - GRID_TICK_1E6)
        out.append(v)
        k += 1
    return tuple(out)


def _line(key: str, values: typing.Sequence[int]) -> str:
    """One artifact line carrying an integer array — ON ONE LINE."""
    return f"{key} = [{', '.join(str(v) for v in values)}]\n"


def _strs(key: str, values: typing.Sequence[str]) -> str:
    return f"{key} = [{', '.join(chr(34) + v + chr(34) for v in values)}]\n"


def hour_table_1e9(values: typing.Sequence[int]) -> tuple[int, ...]:
    """An ``hour_ln_off_1e9`` table, checked: exactly :data:`HOURS` ints,
    each within :data:`HOUR_LN_OFF_ABS_MAX_1E9` of zero."""
    out = tuple(values)
    if len(out) != HOURS:
        raise ValueError(f"hour_ln_off_1e9 must carry exactly {HOURS} integers (got {len(out)})")
    h = 0
    while h < HOURS:
        v = out[h]
        if type(v) is not int:
            raise TypeError(f"hour_ln_off_1e9[{h}] must be an int, got {type(v).__name__}")
        if abs(v) > HOUR_LN_OFF_ABS_MAX_1E9:
            raise ValueError(
                f"hour_ln_off_1e9[{h}] = {v} is past ln 2 x1e9 ({HOUR_LN_OFF_ABS_MAX_1E9}): "
                "an offset that halves or doubles sigma-hat is a different forecast, not "
                "an hour-of-day correction"
            )
        h += 1
    return out


def phi_lut_1e6(
    phi: str = PHI_NORMAL,
    phi_nu: DecimalLike | None = None,
    phi_s: DecimalLike | None = None,
) -> tuple[int, ...]:
    """The ``phi_lut`` of the asked-for price map.

    ``normal`` is Φ and takes no parameters: a ``nu`` or ``s`` that would
    be silently ignored is a fit nobody gets, so it is refused.
    ``student-t`` needs both.
    """
    if phi == PHI_NORMAL:
        if phi_nu is not None or phi_s is not None:
            raise ValueError("phi 'normal' takes no nu or s: Phi has no parameters")
        return phi_table_1e6()
    if phi == PHI_STUDENT_T:
        if phi_nu is None or phi_s is None:
            raise ValueError("phi 'student-t' needs both nu and s")
        return student_t_table_1e6(phi_nu, phi_s)
    raise ValueError(f"phi must be one of {', '.join(PHI_KINDS)}, got {phi!r}")


def render_tables(  # noqa: PLR0913 — the four knobs, then the four fit arguments
    slope_early: int = SLOPE_EARLY_MILLI,
    slope_mid: int = SLOPE_MID_MILLI,
    slope_late: int = SLOPE_LATE_MILLI,
    scale_1e9: int = SCALE_1E9_DEFAULT,
    *,
    phi: str = PHI_NORMAL,
    phi_nu: DecimalLike | None = None,
    phi_s: DecimalLike | None = None,
    hour_ln_off_1e9: typing.Sequence[int] | None = None,
) -> str:
    """The generated key lines only, in artifact order.

    With the defaults these are today's bytes (Φ, no hour table). An hour
    table, when given, is the LAST line: the one after ``scale_1e9``.
    """
    hours = None if hour_ln_off_1e9 is None else hour_table_1e9(hour_ln_off_1e9)
    text = (
        _line("phi_lut", phi_lut_1e6(phi, phi_nu, phi_s))
        + _line("recal_early", recal_table_1e6(slope_early))
        + _line("recal_mid", recal_table_1e6(slope_mid))
        + _line("recal_late", recal_table_1e6(slope_late))
        + f"scale_1e9       = {scale_1e9}\n"
    )
    if hours is not None:
        text += _line("hour_ln_off_1e9", hours)
    return text


def render_artifact(
    families: typing.Sequence[str] = FAMILIES_DEFAULT,
    underlying: typing.Sequence[str] = UNDERLYING_DEFAULT,
    slope_early: int = SLOPE_EARLY_MILLI,
    slope_mid: int = SLOPE_MID_MILLI,
    slope_late: int = SLOPE_LATE_MILLI,
    scale_1e9: int = SCALE_1E9_DEFAULT,
    *,
    phi: str = PHI_NORMAL,
    phi_nu: DecimalLike | None = None,
    phi_s: DecimalLike | None = None,
    hour_ln_off_1e9: typing.Sequence[int] | None = None,
) -> str:
    """A complete ``bin15.toml``.

    Every key the grammar requires, the operator knobs at the spec's
    stated values, and the generated tables. What this writes parses:
    ``core_config::bin15::parse`` accepts it and every bound holds. The
    header moves only where a fit flag was given — the hour paragraph
    with an hour table, the Φ line with a t — so the defaults write
    today's bytes.
    """
    tables = render_tables(
        slope_early,
        slope_mid,
        slope_late,
        scale_1e9,
        phi=phi,
        phi_nu=phi_nu,
        phi_s=phi_s,
        hour_ln_off_1e9=hour_ln_off_1e9,
    )
    if hour_ln_off_1e9 is None:
        hour_note = (
            "# `hour_ln_off_1e9` is ABSENT on purpose: absent means zero, which is\n"
            "# bit-identical to no hour-of-day correction, and there is no fit for one\n"
            "# yet. 24 zeros would say the same thing less honestly.\n"
        )
    else:
        hour_note = (
            "# `hour_ln_off_1e9` is an hour-of-day table: per UTC hour, an offset on\n"
            "# ln(sigma-hat), x1e9. Its 24 values are a FIT, passed to the fitter as\n"
            "# an argument (`--hour-ln-off-1e9`); the fitter keeps none of them.\n"
        )
    if phi == PHI_NORMAL:
        phi_note = "# Phi is computed exactly in `decimal`, not fitted.\n"
    else:
        nu = _decimal_param("nu", typing.cast(DecimalLike, phi_nu))
        s = _decimal_param("s", typing.cast(DecimalLike, phi_s))
        phi_note = (
            "# phi_lut is the Student-t map p(d) = T_nu(k d), k = s sqrt(nu/(nu-2)),\n"
            f"# nu = {nu}, s = {s}. Both are a FIT, passed to the fitter as\n"
            "# arguments (`--phi-nu`, `--phi-s`); T_nu itself is computed exactly\n"
            "# in `decimal`.\n"
        )
    head = (
        "# bin15.toml - the BIN15 member's artifact (slot 3).\n"
        "#\n"
        "# Generated by `python -m claude_worker.bin15_fit artifact`. The engine\n"
        "# hashes these BYTES: the boot tell `bin15: artifact configured hash=...`\n"
        "# names this exact file, so an edit by hand is a new artifact and should\n"
        "# be re-cut through the fitter rather than patched.\n"
        "#\n"
        "# THE ARRAYS ARE ONE LINE EACH, DELIBERATELY. The grammar\n"
        "# (`core_config::icdp::parse_value`, reused by `core_config::bin15`) reads\n"
        "# an array off a single trimmed line; a multi-line array is an\n"
        "# unterminated array to it, not a formatting choice.\n"
        "#\n"
        "# `families` must equal `universe.toml`'s `[hyperliquid] rolling` AS A\n"
        "# SEQUENCE - `bin15_boot` refuses a permutation, because the member\n"
        "# indexes families by the ingress's own index and a permuted list would\n"
        "# price one family's book against another's forecast.\n"
        "#\n"
        + hour_note
        + "#\n"
        + f"# Recalibration slopes (milli-units): early {slope_early} mid {slope_mid}"
        f" late {slope_late}.\n"
        + phi_note
        + "[bin15]\n"
    )
    body = _strs("families", families) + _strs("underlying", underlying)
    for key, value in KNOBS:
        body += f"{key:<15} = {value}\n"
    return head + body + tables


def _decimal_arg(text: str) -> decimal.Decimal:
    """``--phi-nu`` / ``--phi-s``: a decimal STRING, parsed exactly."""
    try:
        value = decimal.Decimal(text)
    except decimal.InvalidOperation as exc:
        raise argparse.ArgumentTypeError(f"not a decimal number: {text!r}") from exc
    if not value.is_finite():
        raise argparse.ArgumentTypeError(f"not a finite number: {text!r}")
    return value


def _hour_arg(text: str) -> tuple[int, ...]:
    """``--hour-ln-off-1e9``: 24 comma-separated integers, checked."""
    try:
        return hour_table_1e9(tuple(int(part) for part in text.split(",")))
    except (TypeError, ValueError) as exc:
        raise argparse.ArgumentTypeError(str(exc)) from exc


def _add_fit_args(ap: argparse.ArgumentParser) -> None:
    ap.add_argument("--slope-early", type=int, default=SLOPE_EARLY_MILLI)
    ap.add_argument("--slope-mid", type=int, default=SLOPE_MID_MILLI)
    ap.add_argument("--slope-late", type=int, default=SLOPE_LATE_MILLI)
    ap.add_argument("--scale-1e9", type=int, default=SCALE_1E9_DEFAULT, dest="scale_1e9")
    ap.add_argument(
        "--phi",
        choices=PHI_KINDS,
        default=PHI_NORMAL,
        help="the price map: Phi (the default) or a Student-t T_nu(k d)",
    )
    ap.add_argument(
        "--phi-nu",
        type=_decimal_arg,
        default=None,
        help="the t's degrees of freedom (over 2), as a decimal string",
    )
    ap.add_argument(
        "--phi-s",
        type=_decimal_arg,
        default=None,
        help="the t's scale against unit variance (over 0), as a decimal string",
    )
    ap.add_argument(
        "--hour-ln-off-1e9",
        type=_hour_arg,
        default=None,
        dest="hour_ln_off_1e9",
        help="24 comma-separated ln(sigma-hat) offsets x1e9, one per UTC hour; "
        "write --hour-ln-off-1e9=... when the first one is negative",
    )


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; the verb surface stays frozen once published."""
    ap = argparse.ArgumentParser(prog="claude_worker.bin15_fit")
    sub = ap.add_subparsers(dest="lane", required=True)

    tab = sub.add_parser("tables", help="the generated key lines only")
    tab.add_argument("--out", type=pathlib.Path, default=None)
    _add_fit_args(tab)

    art = sub.add_parser("artifact", help="a complete bin15.toml")
    art.add_argument("--out", required=True, type=pathlib.Path)
    art.add_argument("--families", nargs="+", default=list(FAMILIES_DEFAULT))
    art.add_argument("--underlying", nargs="+", default=list(UNDERLYING_DEFAULT))
    _add_fit_args(art)

    args = ap.parse_args(argv)
    lane = tab if args.lane == "tables" else art
    if args.phi == PHI_STUDENT_T and (args.phi_nu is None or args.phi_s is None):
        lane.error("--phi student-t needs both --phi-nu and --phi-s")
    if args.phi == PHI_NORMAL and (args.phi_nu is not None or args.phi_s is not None):
        lane.error("--phi-nu and --phi-s shape a Student-t map: pass --phi student-t")
    if args.lane == "tables":
        text = render_tables(
            args.slope_early,
            args.slope_mid,
            args.slope_late,
            args.scale_1e9,
            phi=args.phi,
            phi_nu=args.phi_nu,
            phi_s=args.phi_s,
            hour_ln_off_1e9=args.hour_ln_off_1e9,
        )
    else:
        text = render_artifact(
            args.families,
            args.underlying,
            args.slope_early,
            args.slope_mid,
            args.slope_late,
            args.scale_1e9,
            phi=args.phi,
            phi_nu=args.phi_nu,
            phi_s=args.phi_s,
            hour_ln_off_1e9=args.hour_ln_off_1e9,
        )
    if getattr(args, "out", None) is None:
        sys.stdout.write(text)
        return 0
    out = typing.cast(pathlib.Path, args.out)
    tmp = out.with_suffix(out.suffix + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(out)
    phi = phi_lut_1e6(args.phi, args.phi_nu, args.phi_s)
    kind = args.phi if args.phi == PHI_NORMAL else f"{args.phi} nu {args.phi_nu} s {args.phi_s}"
    hours = "none" if args.hour_ln_off_1e9 is None else str(HOURS)
    print(
        f"bin15-fit: {args.lane} -> {out} (phi {kind} {len(phi)} pts "
        f"[{phi[0]}..{phi[-1]}] recal 3 x {RECAL_POINTS} scale {args.scale_1e9} "
        f"hours {hours})",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
