# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_ref.py -- the BIN15 pricer mirror (O4b).

Two jobs. The first is the shared fixtures: replay
``tests/fixtures/bin15/parity-<n>.input.tsv`` and assert every row against
``parity-<n>.expected.tsv``, which ``crates/strategy-bin15/tests/parity.rs``
writes (``parity-2`` carries the BIN15 S3 lanes: the venue's settlement
law in the pricer). **Tolerance is zero** -- the pricer is all-integer, so a
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


@pytest.mark.parametrize("name", ["parity-1", "parity-2"])
def test_the_shared_fixture_matches_the_engine_bit_for_bit(name: str) -> None:
    """The parity gate. Regenerate with
    ``BIN15_PARITY_WRITE=<name> cargo nextest run -p strategy-bin15 --test parity``
    -- never by editing the expected file."""
    got = claude_worker.bin15_ref.replay(
        (_FIXTURES / f"{name}.input.tsv").read_text(encoding="utf-8").splitlines()
    )
    want = [
        line.strip()
        for line in (_FIXTURES / f"{name}.expected.tsv").read_text(encoding="utf-8").splitlines()
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


def test_parity_2_covers_every_s3_record_and_its_refusals() -> None:
    """The S3 tape exercises all four new records and both sides of every
    branch the running-TWAP law has."""
    got = claude_worker.bin15_ref.replay(
        (_FIXTURES / "parity-2.input.tsv").read_text(encoding="utf-8").splitlines()
    )
    tags = {row.split("\t")[0] for row in got}
    assert tags == {"T", "S", "M", "W"}
    assert any(row == "W\t-\t-\t-\t-\t-" for row in got), "no refused re-price in the tape"
    assert any(row == "M\t-" for row in got), "no refused moneyness in the tape"
    assert any(row == "S\t0\t0" for row in got), "no empty piece in the tape"
    d_clamp = claude_worker.bin15_ref.D_CLAMP_1E6
    ws = [row.split("\t") for row in got if row.startswith("W\t") and "-" not in row.split("\t")[1]]
    assert any(int(w[3]) == d_clamp and int(w[4]) == 0 for w in ws), "nothing-left: + clamp"
    assert any(int(w[3]) == -d_clamp and int(w[4]) == 0 for w in ws), "nothing-left: - clamp"


# --- BIN15 S3: the venue's settlement law in the pricer ----------------

_S: int = 1_000_000_000
_W: int = 60 * _S


def test_the_horizon_is_the_variance_time_of_a_twap_ending_at_expiry() -> None:
    h = claude_worker.bin15_ref.pricing_horizon_ns
    assert h(900 * _S, _W) == 860 * _S, "tau - 2W/3 with the window ahead"
    assert h(_W, _W) == 20 * _S, "W/3 at the open"
    assert h(30 * _S, _W) == 2_500_000_000, "30^3 / (3 * 60^2) = 2.5 s inside"
    assert abs(h(_W - 1, _W) - 20 * _S) < 2, "continuous at the open"
    assert h(0, _W) == 0
    assert h(123_456, 0) == 123_456, "a family settling AT T keeps tau"
    w_max = 65_535 * _S
    assert h(2**64 - 1, w_max) == 2**64 - 1 - (2 * w_max) // 3
    # Monotone in the time left, both branches.
    prev = -1
    t = 0
    while t <= 2 * _W:
        cur = h(t, _W)
        assert cur >= prev
        prev = cur
        t += 997_000_003


def test_the_segment_is_clipped_to_the_window_and_time_weighted() -> None:
    seg = claude_worker.bin15_ref.binary_twap_segment
    o, c = 100 * _S, 160 * _S
    assert seg(7, 90 * _S, 110 * _S, o, c) == (7 * 10 * _S, 10 * _S), "clipped at the open"
    assert seg(7, 150 * _S, 170 * _S, o, c) == (7 * 10 * _S, 10 * _S), "clipped at the close"
    assert seg(7, 80 * _S, 90 * _S, o, c) == (0, 0), "before the window"
    assert seg(7, 120 * _S, 120 * _S, o, c) == (0, 0), "no length"
    assert claude_worker.bin15_ref.binary_settle_open_ns(c, 60 * _S) == o
    assert claude_worker.bin15_ref.binary_settle_open_ns(5, 60 * _S) == 0, "saturating"


def test_inside_the_window_the_known_average_decides() -> None:
    lut = _shipped_luts()
    fv = claude_worker.bin15_ref.fair_value_twap
    k = _STRIKE
    above = (k + k // 200) * 40 * _S
    below = (k - k // 200) * 40 * _S
    win = fv(lut, k - k // 1_000, k, 20 * _S, _W, above, _SIG2)
    lose = fv(lut, k + k // 1_000, k, 20 * _S, _W, below, _SIG2)
    assert win is not None and lose is not None
    assert win.p_hat_1e6 > 990_000, "40 s known above outweighs a 0.1 % dip"
    assert lose.p_hat_1e6 < 10_000
    # Nothing left: the known average IS the settlement, `>=` is ITM.
    tie = fv(lut, k, k, 0, _W, k * _W, _SIG2)
    under = fv(lut, k, k, 0, _W, k * _W - 1, _SIG2)
    assert tie is not None and under is not None
    assert tie.d_1e6 == claude_worker.bin15_ref.D_CLAMP_1E6 and tie.den_1e9 == 0
    assert under.d_1e6 == -claude_worker.bin15_ref.D_CLAMP_1E6
    # Outside the window it IS fair_value at the horizon.
    out = fv(lut, k + 50_000_000, k, 120 * _S, _W, 0, _SIG2)
    ref = claude_worker.bin15_ref.fair_value(lut, k + 50_000_000, k, 80 * _S, _SIG2)
    assert out == ref
    # Absent data holds on both sides of the window.
    assert fv(lut, k, k, 20 * _S, _W, above, 0) is None
    assert fv(lut, 0, k, 20 * _S, _W, above, _SIG2) is None
    assert fv(lut, k, k, 20 * _S, _W, 2**127 - 1, _SIG2) is None, "overflow refuses"


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
        # BIN15 P1a (F1): the SHIPPED slopes are identity (1.000), so a
        # raw 0.75 stays 0.75. The withdrawn 1.104 / 1.165 / 1.219
        # pushed it outward -- and pushed the tail buckets clean off the
        # unit interval, which is why they are gone. This assertion
        # follows the artifact, so a re-fit that ships a sharper slope
        # is a deliberate edit here.
        assert lut.recal_1e6(phase, 750_000) == 750_000, "identity leaves it alone"


def test_no_shipped_recal_bucket_pins_interior_certainty() -> None:
    """BIN15 P1a (F1): the mirror's own copy of the shipped tables.

    ``core_config::bin15::table`` refuses an artifact whose interior
    buckets are 0 or 1e6, and the fitter cannot build one. This is the
    third place the law is checked, on the numbers the mirror actually
    prices with -- because the failure it prevents (``p_hat = 1.000000``
    off a raw 0.984) is invisible in every counter the member has.
    """
    lut = _shipped_luts()
    for phase in range(claude_worker.bin15_ref.PHASES):
        table = lut.recal[phase]
        assert table[0] == 0 and table[-1] == 1_000_000, "only the ends are certain"
        for k in range(1, len(table) - 1):
            assert 0 < table[k] < 1_000_000, f"recal[{phase}][{k}] = {table[k]}"


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
