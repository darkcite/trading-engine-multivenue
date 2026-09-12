# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15.toml's lookup tables — the BIN15 pricer's fitted data (O4b).

``crates/strategy-bin15/src/price.rs`` is pure integer arithmetic over
three tables it never computes: ``phi_lut`` (the standard normal CDF)
and ``recal_early`` / ``recal_mid`` / ``recal_late`` (the walk-forward
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
* The recalibration is the fitted part, and the fit is three numbers:
  the slopes **1.104 / 1.165 / 1.219** (early / mid / late), which the
  backtest measured as the under-confidence of the raw lognormal price.
  The table is that slope through the middle of the unit interval,
  clamped at both ends: ``p' = clamp(0.5 + slope * (p - 0.5), 0, 1)``.
  A slope over 1 pushes confidence OUTWARD, which is what an
  under-confident model needs.
* ``scale_1e9`` is the stated variance-ratio scale, **0.98**.
* ``hour_ln_off_1e9`` is OMITTED. The parser's law is "absent optional =
  bit-identical to the stated default", and the stated default is zero,
  so an artifact that carries 24 zeros and one that carries nothing are
  the same member — but only the second one is honest about not having
  an hour-of-day fit yet.

Re-fitting later is an artifact edit and a restart, never a code
change: pass ``--slope-early`` / ``--slope-mid`` / ``--slope-late``
(milli-units, so 1104 is 1.104) and ``--scale-1e9``.

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

#: Fitted recalibration slopes in MILLI-units, per phase. 1104 is
#: 1.104. Integers because the artifact is integer-only and a slope
#: spelled as a float would be rounded somewhere invisible.
SLOPE_EARLY_MILLI: int = 1_104
SLOPE_MID_MILLI: int = 1_165
SLOPE_LATE_MILLI: int = 1_219

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
    ("cap_day_usd_1e6", 5_000_000_000),
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
    ``[4096] == 999_979`` — comfortably over the parser's 999_900 floor.
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


def recal_table_1e6(slope_milli: int) -> tuple[int, ...]:
    """``p' ×1e6`` at ``p = 0, 1/64, …, 1`` for one fitted slope.

    ``p' = clamp(0.5 + slope * (p - 0.5), 0, 1)``. The clamp is what
    pins both ends for any slope over 1, which is exactly the shape
    ``core_config::bin15::table`` demands (``[0] == 0``,
    ``[64] == 1e6``, monotone).
    """
    if slope_milli <= 0:
        raise ValueError(f"slope_milli must be positive, got {slope_milli}")
    out: list[int] = []
    k = 0
    while k < RECAL_POINTS:
        p = k * RECAL_STEP_1E6
        v = 500_000 + round_half_away((p - 500_000) * slope_milli, 1_000)
        out.append(min(max(v, 0), 1_000_000))
        k += 1
    return tuple(out)


def _line(key: str, values: typing.Sequence[int]) -> str:
    """One artifact line carrying an integer array — ON ONE LINE."""
    return f"{key} = [{', '.join(str(v) for v in values)}]\n"


def _strs(key: str, values: typing.Sequence[str]) -> str:
    return f"{key} = [{', '.join(chr(34) + v + chr(34) for v in values)}]\n"


def render_tables(
    slope_early: int = SLOPE_EARLY_MILLI,
    slope_mid: int = SLOPE_MID_MILLI,
    slope_late: int = SLOPE_LATE_MILLI,
    scale_1e9: int = SCALE_1E9_DEFAULT,
) -> str:
    """The generated key lines only, in artifact order."""
    phi = phi_table_1e6()
    return (
        _line("phi_lut", phi)
        + _line("recal_early", recal_table_1e6(slope_early))
        + _line("recal_mid", recal_table_1e6(slope_mid))
        + _line("recal_late", recal_table_1e6(slope_late))
        + f"scale_1e9       = {scale_1e9}\n"
    )


def render_artifact(
    families: typing.Sequence[str] = FAMILIES_DEFAULT,
    underlying: typing.Sequence[str] = UNDERLYING_DEFAULT,
    slope_early: int = SLOPE_EARLY_MILLI,
    slope_mid: int = SLOPE_MID_MILLI,
    slope_late: int = SLOPE_LATE_MILLI,
    scale_1e9: int = SCALE_1E9_DEFAULT,
) -> str:
    """A complete ``bin15.toml``.

    Every key the grammar requires, the operator knobs at the spec's
    stated values, and the generated tables. What this writes parses:
    ``core_config::bin15::parse`` accepts it and every bound holds.
    """
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
        "# `hour_ln_off_1e9` is ABSENT on purpose: absent means zero, which is\n"
        "# bit-identical to no hour-of-day correction, and there is no fit for one\n"
        "# yet. 24 zeros would say the same thing less honestly.\n"
        "#\n"
        f"# Recalibration slopes (milli-units): early {slope_early} mid {slope_mid}"
        f" late {slope_late}.\n"
        "# Phi is computed exactly in `decimal`, not fitted.\n"
        "[bin15]\n"
    )
    body = _strs("families", families) + _strs("underlying", underlying)
    for key, value in KNOBS:
        body += f"{key:<15} = {value}\n"
    return head + body + render_tables(slope_early, slope_mid, slope_late, scale_1e9)


def _add_fit_args(ap: argparse.ArgumentParser) -> None:
    ap.add_argument("--slope-early", type=int, default=SLOPE_EARLY_MILLI)
    ap.add_argument("--slope-mid", type=int, default=SLOPE_MID_MILLI)
    ap.add_argument("--slope-late", type=int, default=SLOPE_LATE_MILLI)
    ap.add_argument("--scale-1e9", type=int, default=SCALE_1E9_DEFAULT, dest="scale_1e9")


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
    if args.lane == "tables":
        text = render_tables(args.slope_early, args.slope_mid, args.slope_late, args.scale_1e9)
    else:
        text = render_artifact(
            args.families,
            args.underlying,
            args.slope_early,
            args.slope_mid,
            args.slope_late,
            args.scale_1e9,
        )
    if getattr(args, "out", None) is None:
        sys.stdout.write(text)
        return 0
    out = typing.cast(pathlib.Path, args.out)
    tmp = out.with_suffix(out.suffix + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(out)
    phi = phi_table_1e6()
    print(
        f"bin15-fit: {args.lane} -> {out} (phi {len(phi)} pts "
        f"[{phi[0]}..{phi[-1]}] recal 3 x {RECAL_POINTS} scale {args.scale_1e9})",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
