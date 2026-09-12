# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_ref.py -- the BIN15 pricer mirror (O4b).

Two jobs. The first is the shared fixture: replay
``tests/fixtures/bin15/parity-1.input.tsv`` and assert every row against
``parity-1.expected.tsv``, which ``crates/strategy-bin15/tests/parity.rs``
writes. **Tolerance is zero** -- the pricer is all-integer, so a
one-unit disagreement is a different model, and the fair value is what
decides whether the member crosses a book.

The second is the properties the fixture cannot state: that Phi is a
real CDF rather than any monotone ramp, that the price is a half at the
money and monotone in the mark, that a longer horizon pulls toward a
half, and that absent data HOLDS instead of pricing.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import pytest

import claude_worker.bin15_fit
import claude_worker.bin15_ref

_FIXTURES: pathlib.Path = pathlib.Path(__file__).parent / "fixtures" / "bin15"

#: sigma_min = 10 bps of log-price per minute => sigma^2 x1e18 = 1e12.
_SIG2: int = 1_000_000_000_000
_STRIKE: int = 79_000_000_000
_TAU_10M: int = 600_000_000_000


def _shipped_luts() -> claude_worker.bin15_ref.Bin15Luts:
    """The tables the artifact ships, as the pricer sees them."""
    return claude_worker.bin15_ref.Bin15Luts(
        claude_worker.bin15_fit.phi_table_1e6(),
        (
            claude_worker.bin15_fit.recal_table_1e6(claude_worker.bin15_fit.SLOPE_EARLY_MILLI),
            claude_worker.bin15_fit.recal_table_1e6(claude_worker.bin15_fit.SLOPE_MID_MILLI),
            claude_worker.bin15_fit.recal_table_1e6(claude_worker.bin15_fit.SLOPE_LATE_MILLI),
        ),
    )


def test_the_shared_fixture_matches_the_engine_bit_for_bit() -> None:
    """The parity gate. Regenerate with
    ``BIN15_PARITY_WRITE=parity-1 cargo nextest run -p strategy-bin15 --test parity``
    -- never by editing the expected file."""
    got = claude_worker.bin15_ref.replay(
        (_FIXTURES / "parity-1.input.tsv").read_text(encoding="utf-8").splitlines()
    )
    want = [
        line.strip()
        for line in (_FIXTURES / "parity-1.expected.tsv").read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    ]
    assert len(got) > 1_000, f"the fixture produced almost nothing ({len(got)} rows)"
    assert len(got) == len(want), "row count moved -- the tape changed, not the law"
    for i, (g, w) in enumerate(zip(got, want, strict=True)):
        assert g == w, f"row {i + 1} disagrees with the engine (tolerance is 0): {g!r} != {w!r}"


def test_the_fixture_covers_every_record_the_grammar_has() -> None:
    """A tape that silently stopped exercising a branch is a green that
    means nothing."""
    got = claude_worker.bin15_ref.replay(
        (_FIXTURES / "parity-1.input.tsv").read_text(encoding="utf-8").splitlines()
    )
    tags = {row.split("\t")[0] for row in got}
    assert tags == {"F", "X", "P", "R", "H", "G"}
    # And it exercises the refusal path, not just the happy one.
    assert any(row == "F\t-\t-\t-" for row in got), "no refused re-price in the tape"
    assert any(row == "X\t-" for row in got), "no refused moneyness in the tape"


def test_the_identity_tables_are_the_identity() -> None:
    lut = claude_worker.bin15_ref.Bin15Luts.identity()
    assert lut.phi_1e6(0) == 500_000
    assert lut.phi_1e6(claude_worker.bin15_ref.D_CLAMP_1E6) == 500_000
    for k in range(claude_worker.bin15_ref.RECAL_POINTS):
        p = k * claude_worker.bin15_ref.RECAL_STEP_1E6
        assert lut.recal_1e6(claude_worker.bin15_ref.PHASE_EARLY, p) == p


def test_phi_is_a_real_cdf_and_interpolates_off_grid() -> None:
    lut = _shipped_luts()
    assert lut.phi_1e6(0) == 500_000
    assert lut.phi_1e6(1_000_000) == 841_345
    assert lut.phi_1e6(1_960_000) == 975_002
    assert lut.phi_1e6(3_000_000) == 998_650
    # Halfway between two grid points is halfway between two values.
    lo = lut.phi_1e6(1_000_000)
    hi = lut.phi_1e6(1_001_000)
    assert lut.phi_1e6(1_000_500) == lo + (hi - lo) // 2
    # Monotone off the grid too.
    prev = lut.phi_1e6(0)
    d = 0
    while d <= claude_worker.bin15_ref.D_CLAMP_1E6:
        v = lut.phi_1e6(d)
        assert v >= prev, f"Phi fell at d={d}"
        prev = v
        d += 137
    with pytest.raises(ValueError):
        lut.phi_1e6(-1)
    with pytest.raises(ValueError):
        lut.phi_1e6(claude_worker.bin15_ref.D_CLAMP_1E6 + 1)


def test_recal_pins_both_ends_and_clamps_out_of_range() -> None:
    lut = _shipped_luts()
    for phase in range(claude_worker.bin15_ref.PHASES):
        assert lut.recal_1e6(phase, 0) == 0
        assert lut.recal_1e6(phase, 1_000_000) == 1_000_000
        assert lut.recal_1e6(phase, 500_000) == 500_000, "the middle is fixed"
        assert lut.recal_1e6(phase, -5) == 0
        assert lut.recal_1e6(phase, 2_000_000) == 1_000_000
        assert lut.recal_1e6(phase, 750_000) > 750_000, "a slope over 1 pushes outward"
    # Later phases are MORE under-confident, so they push harder.
    early = lut.recal_1e6(claude_worker.bin15_ref.PHASE_EARLY, 750_000)
    mid = lut.recal_1e6(claude_worker.bin15_ref.PHASE_MID, 750_000)
    late = lut.recal_1e6(claude_worker.bin15_ref.PHASE_LATE, 750_000)
    assert early < mid < late


def test_the_phase_boundaries_are_where_the_spec_says() -> None:
    assert claude_worker.bin15_ref.phase_of(900_000_000_000) == claude_worker.bin15_ref.PHASE_EARLY
    assert (
        claude_worker.bin15_ref.phase_of(claude_worker.bin15_ref.PHASE_EARLY_NS)
        == claude_worker.bin15_ref.PHASE_EARLY
    ), "10 min is early"
    assert (
        claude_worker.bin15_ref.phase_of(claude_worker.bin15_ref.PHASE_EARLY_NS - 1)
        == claude_worker.bin15_ref.PHASE_MID
    )
    assert (
        claude_worker.bin15_ref.phase_of(claude_worker.bin15_ref.PHASE_MID_NS)
        == claude_worker.bin15_ref.PHASE_MID
    ), "4 min is mid"
    assert (
        claude_worker.bin15_ref.phase_of(claude_worker.bin15_ref.PHASE_MID_NS - 1)
        == claude_worker.bin15_ref.PHASE_LATE
    )
    assert claude_worker.bin15_ref.phase_of(0) == claude_worker.bin15_ref.PHASE_LATE


def test_log_moneyness_is_second_order_and_refuses_nonsense() -> None:
    f = claude_worker.bin15_ref.log_moneyness_1e9
    assert f(_STRIKE, _STRIKE) == 0
    # ln(1.01) = 0.00995033; the 2nd-order form gives 0.00995.
    x = f(79_790_000_000, _STRIKE)
    assert x is not None and abs(x - 9_950_331) < 400
    x = f(78_210_000_000, _STRIKE)
    assert x is not None and abs(x + 10_050_336) < 400
    assert f(0, _STRIKE) is None
    assert f(_STRIKE, 0) is None
    assert f(-1, 1) is None
    # The U_CLAMP band: the square would overflow i128 past it, and a
    # mark a hundred times its strike is a bad strike.
    assert f(101_000_000, 1_000_000) is not None, "u = 1e11 is inside"
    assert f(102_000_000, 1_000_000) is None, "u > 1e11 is outside"
    assert f(1_000_000_000_000_000_000, 1) is None
    assert f(_STRIKE, 1) is None


def test_fair_value_is_a_half_at_the_money_and_monotone_in_the_mark() -> None:
    lut = _shipped_luts()
    fair = claude_worker.bin15_ref.fair_value(lut, _STRIKE, _STRIKE, _TAU_10M, _SIG2)
    assert fair is not None
    assert fair.d_1e6 == 0
    assert fair.p_raw_1e6 == 500_000, "at the money a binary is a coin flip"
    assert fair.p_hat_1e6 == 500_000, "and the recalibration fixes the middle"
    prev = 0
    bump = -400_000_000
    while bump <= 400_000_000:
        fair = claude_worker.bin15_ref.fair_value(lut, _STRIKE + bump, _STRIKE, _TAU_10M, _SIG2)
        assert fair is not None
        assert fair.p_hat_1e6 >= prev, f"p fell at bump={bump}"
        prev = fair.p_hat_1e6
        bump += 10_000_000
    assert prev > 500_000, "and it did move"


def test_a_longer_horizon_pulls_the_price_toward_a_half() -> None:
    lut = _shipped_luts()
    mark = _STRIKE + 200_000_000  # ~25 bps above
    near = claude_worker.bin15_ref.fair_value(lut, mark, _STRIKE, 60_000_000_000, _SIG2)
    far = claude_worker.bin15_ref.fair_value(lut, mark, _STRIKE, 900_000_000_000, _SIG2)
    assert near is not None and far is not None
    assert near.p_hat_1e6 > far.p_hat_1e6
    assert far.p_hat_1e6 > 500_000, "but still above a coin flip"


def test_absent_data_holds_and_the_clamp_is_symmetric() -> None:
    lut = _shipped_luts()
    fv = claude_worker.bin15_ref.fair_value
    assert fv(lut, _STRIKE, _STRIKE, 0, _SIG2) is None, "no horizon"
    assert fv(lut, _STRIKE, _STRIKE, _TAU_10M, 0) is None, "cold forecast"
    assert fv(lut, _STRIKE, _STRIKE, _TAU_10M, -1) is None
    assert fv(lut, 0, _STRIKE, _TAU_10M, _SIG2) is None, "no mark"
    # A variance whose root floors to zero is not a forecast.
    assert fv(lut, _STRIKE, _STRIKE, 1_000_000_000, 1) is None
    hi = fv(lut, _STRIKE * 2, _STRIKE, _TAU_10M, 1)
    lo = fv(lut, _STRIKE // 2, _STRIKE, _TAU_10M, 1)
    assert hi is not None and lo is not None
    assert hi.d_1e6 == claude_worker.bin15_ref.D_CLAMP_1E6
    assert lo.d_1e6 == -claude_worker.bin15_ref.D_CLAMP_1E6
    assert hi.p_raw_1e6 + lo.p_raw_1e6 == 1_000_000, "the two ends are reflections"


def test_the_grid_rounders_go_the_right_way_on_both_signs() -> None:
    floor = claude_worker.bin15_ref.floor_grid_1e6
    ceil = claude_worker.bin15_ref.ceil_grid_1e6
    assert floor(400_050, 100) == 400_000
    assert ceil(400_050, 100) == 400_100
    assert floor(400_000, 100) == 400_000
    assert ceil(400_000, 100) == 400_000
    assert floor(-50, 100) % 100 == 0
    assert ceil(-50, 100) % 100 == 0
    assert floor(-50, 100) <= -50
    assert ceil(-50, 100) >= -50
    with pytest.raises(ValueError):
        floor(100, 0)
    with pytest.raises(ValueError):
        ceil(100, -1)


def test_the_i64_wrap_is_mirrored_not_approximated() -> None:
    """Rust's ``as i64`` truncates; a mirror that widened it silently
    would agree everywhere except where it matters."""
    wrap = claude_worker.bin15_ref._wrap_i64
    assert wrap(0) == 0
    assert wrap(claude_worker.bin15_ref.I64_MAX) == claude_worker.bin15_ref.I64_MAX
    assert wrap(claude_worker.bin15_ref.I64_MAX + 1) == claude_worker.bin15_ref.I64_MIN
    assert wrap(claude_worker.bin15_ref.I64_MIN - 1) == claude_worker.bin15_ref.I64_MAX
    assert wrap(2**64) == 0


def test_floor_div_and_isqrt_mirror_the_engines_primitives() -> None:
    assert claude_worker.bin15_ref.floor_div(7, 2) == 3
    assert claude_worker.bin15_ref.floor_div(-7, 2) == -4, "toward minus infinity"
    with pytest.raises(ValueError):
        claude_worker.bin15_ref.floor_div(1, 0)
    assert claude_worker.bin15_ref.isqrt_i128(0) == 0
    assert claude_worker.bin15_ref.isqrt_i128(-5) == 0
    assert claude_worker.bin15_ref.isqrt_i128(15) == 3
    assert claude_worker.bin15_ref.isqrt_i128(16) == 4
    # 2^126's root is 2^63, one past i64 -- the engine saturates there.
    assert claude_worker.bin15_ref.isqrt_i128(2**126) == claude_worker.bin15_ref.I64_MAX
