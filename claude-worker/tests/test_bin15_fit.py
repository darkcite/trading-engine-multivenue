# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_fit.py -- the artifact's lookup tables (O4b).

Three things are pinned here. That Phi is the real standard normal CDF
to 1e-6 and not merely a monotone ramp. That the recalibration tables
carry the STATED slopes, pin both ends, and stay antisymmetric about
``(0.5, 0.5)`` -- without which Yes and No do not price to one. And that
the committed ``bin15.toml.example`` is what this module produces TODAY:
a drift between the fitter and the example is a fitter nobody can use to
reproduce the artifact the engine hashes.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import pytest

import claude_worker.bin15_fit

_ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[2]

#: Phi to 1e-6 at a few points a table cannot fake.
_KNOWN: tuple[tuple[int, int], ...] = (
    (0, 500_000),
    (1_000, 841_345),
    (1_960, 975_002),
    (2_576, 995_002),
    (3_000, 998_650),
    (4_096, 999_979),
)


def test_phi_is_the_standard_normal_cdf_to_a_millionth() -> None:
    phi = claude_worker.bin15_fit.phi_table_1e6()
    assert len(phi) == claude_worker.bin15_fit.PHI_POINTS
    for idx, want in _KNOWN:
        assert phi[idx] == want, f"Phi({idx / 1000}) = {phi[idx]}, wanted {want}"


def test_phi_is_monotone_and_inside_the_parsers_bounds() -> None:
    phi = claude_worker.bin15_fit.phi_table_1e6()
    assert all(phi[i + 1] >= phi[i] for i in range(len(phi) - 1))
    assert min(phi) >= 0 and max(phi) <= 1_000_000
    # `core_config::bin15::table` demands exactly these two.
    assert phi[0] == 500_000
    assert phi[-1] >= 999_900


def test_phi_is_strictly_increasing_where_the_cdf_still_moves() -> None:
    """The far tail rounds flat at 1e-6, which is fine; the body must
    not, or the interpolation has dead intervals the pricer would read
    as a certainty."""
    phi = claude_worker.bin15_fit.phi_table_1e6()
    flat = [i for i in range(3_000) if phi[i + 1] == phi[i]]
    assert not flat, f"Phi is flat inside d < 3 at indices {flat[:5]}"


@pytest.mark.parametrize(
    ("slope", "at_075"),
    [
        # BIN15 P1a (F1): the shipped slopes are IDENTITY. 0.5 + 0.25 s
        # at s = 1.000 is 0.75; at s = 1.032 (the largest slope the
        # function will build) it is 0.758.
        (claude_worker.bin15_fit.SLOPE_EARLY_MILLI, 750_000),
        (claude_worker.bin15_fit.SLOPE_MID_MILLI, 750_000),
        (claude_worker.bin15_fit.SLOPE_LATE_MILLI, 750_000),
        (1_032, 758_000),
    ],
)
def test_each_recal_table_carries_its_stated_slope(slope: int, at_075: int) -> None:
    t = claude_worker.bin15_fit.recal_table_1e6(slope)
    assert len(t) == claude_worker.bin15_fit.RECAL_POINTS
    assert t[0] == 0 and t[-1] == 1_000_000, "both ends pinned by the clamp"
    assert t[32] == 500_000, "the middle is fixed"
    assert all(t[i + 1] >= t[i] for i in range(len(t) - 1)), "monotone"
    # p = 0.75 under slope s lands at 0.5 + 0.25 s.
    assert t[48] == at_075


def test_no_interior_bucket_may_be_certain() -> None:
    """BIN15 P1a (F1): only ``[0]`` and ``[64]`` may be 0 or 1e6.

    The withdrawn 1.104 / 1.165 / 1.219 slopes clamped four interior
    buckets flat at 1e6, so the member published ``p_hat = 1.000000``
    off a raw price of 0.984 — a certainty the pricer never held, and
    one that beats every ask the venue can quote. ``core_config::bin15``
    now refuses such an artifact at the grammar; this is the same law on
    the side that WRITES it.
    """
    for slope in (1, 500, 1_000, 1_032):
        t = claude_worker.bin15_fit.recal_table_1e6(slope)
        assert t[0] == 0 and t[-1] == 1_000_000
        for k in range(1, claude_worker.bin15_fit.RECAL_POINTS - 1):
            assert (
                claude_worker.bin15_fit.GRID_TICK_1E6
                <= t[k]
                <= 1_000_000 - claude_worker.bin15_fit.GRID_TICK_1E6
            ), f"slope {slope} bucket {k} = {t[k]}"


def test_a_slope_that_runs_off_the_interval_is_refused_not_clamped() -> None:
    """The three WITHDRAWN slopes, by name, plus the exact bound.

    Clamping such a slope is the silent failure: the table still parses,
    still looks monotone, and quietly asserts certainty. So the fitter
    refuses to build one at all.
    """
    for slope in (1_104, 1_165, 1_219, 1_033):
        with pytest.raises(ValueError, match="pin interior certainty"):
            claude_worker.bin15_fit.recal_table_1e6(slope)
    # 1032 is the largest that stays inside, and it builds.
    assert claude_worker.bin15_fit.recal_table_1e6(1_032)[63] < 1_000_000


def test_every_recal_table_is_antisymmetric_so_yes_and_no_price_to_one() -> None:
    for slope in (
        claude_worker.bin15_fit.SLOPE_EARLY_MILLI,
        claude_worker.bin15_fit.SLOPE_MID_MILLI,
        claude_worker.bin15_fit.SLOPE_LATE_MILLI,
    ):
        t = claude_worker.bin15_fit.recal_table_1e6(slope)
        n = claude_worker.bin15_fit.RECAL_POINTS
        for k in range(n):
            assert t[k] + t[n - 1 - k] == 1_000_000, f"slope {slope} breaks at [{k}]"


def test_a_nonsense_slope_is_refused() -> None:
    with pytest.raises(ValueError):
        claude_worker.bin15_fit.recal_table_1e6(0)
    with pytest.raises(ValueError):
        claude_worker.bin15_fit.recal_table_1e6(-1_104)


def test_round_half_away_is_symmetric_about_zero() -> None:
    f = claude_worker.bin15_fit.round_half_away
    assert f(500, 1_000) == 1
    assert f(-500, 1_000) == -1, "away from zero, so the table stays symmetric"
    assert f(499, 1_000) == 0
    assert f(-499, 1_000) == 0
    assert f(1_500, 1_000) == 2
    assert f(-1_500, 1_000) == -2
    assert f(0, 1_000) == 0
    with pytest.raises(ValueError):
        f(1, 0)


def test_every_array_is_on_one_line_because_the_grammar_is_line_based() -> None:
    """``core_config::icdp::parse_value`` reads an array off ONE trimmed
    line. A pretty-printed multi-line array is an unterminated array to
    it, not a formatting preference."""
    text = claude_worker.bin15_fit.render_artifact()
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#") or "=" not in stripped:
            continue
        value = stripped.split("=", 1)[1].strip()
        if value.startswith("["):
            assert value.endswith("]"), f"array not closed on its own line: {stripped[:40]}"
    # And the arrays are all there, one line each.
    keys = [ln.split("=", 1)[0].strip() for ln in text.splitlines() if "=" in ln and ln[:1] != "#"]
    for key in ("phi_lut", "recal_early", "recal_mid", "recal_late"):
        assert keys.count(key) == 1, f"{key} must appear exactly once"


def test_the_artifact_states_the_scale_and_omits_the_hour_table() -> None:
    text = claude_worker.bin15_fit.render_artifact()
    assert "scale_1e9" in text
    assert f"scale_1e9       = {claude_worker.bin15_fit.SCALE_1E9_DEFAULT}" in text
    # Absent means zero, which is bit-identical to no correction -- and
    # there is no hour-of-day fit yet, so saying nothing is the honest
    # spelling. `core_config::bin15` pins the same law from its side.
    # The header EXPLAINS the omission, so look at the key lines only.
    keys = {
        ln.split("=", 1)[0].strip()
        for ln in text.splitlines()
        if "=" in ln and not ln.lstrip().startswith("#")
    }
    assert "hour_ln_off_1e9" not in keys
    # Ruling O-Q7's eight families and four underlyings.
    assert text.count("out:") == 4
    assert text.count("native:") == 4
    assert text.count("hyperliquid:") == 4


def test_the_committed_example_is_what_the_fitter_produces_today() -> None:
    """The drift guard. ``core_config::bin15`` compiles the example in
    and asserts it parses; this side asserts it is REPRODUCIBLE, so an
    operator re-cutting the artifact gets the file the tests describe."""
    example = _ROOT / "bin15.toml.example"
    assert example.is_file(), f"{example} is the grammar's contract and must be committed"
    assert example.read_text(encoding="utf-8") == claude_worker.bin15_fit.render_artifact()


def test_the_tables_lane_emits_only_the_generated_keys() -> None:
    text = claude_worker.bin15_fit.render_tables()
    keys = {ln.split("=", 1)[0].strip() for ln in text.splitlines() if "=" in ln}
    assert keys == {"phi_lut", "recal_early", "recal_mid", "recal_late", "scale_1e9"}


def test_the_cli_writes_an_artifact_atomically(tmp_path: pathlib.Path) -> None:
    out = tmp_path / "bin15.toml"
    assert claude_worker.bin15_fit.main(["artifact", "--out", str(out)]) == 0
    assert out.read_text(encoding="utf-8") == claude_worker.bin15_fit.render_artifact()
    assert not list(tmp_path.glob("*.tmp")), "the temp file must be renamed away"
    # A re-fit is a flag, not a code change.
    assert (
        claude_worker.bin15_fit.main(
            ["artifact", "--out", str(out), "--slope-early", "1020", "--scale-1e9", "1000000000"]
        )
        == 0
    )
    text = out.read_text(encoding="utf-8")
    assert "scale_1e9       = 1000000000" in text
    assert text != claude_worker.bin15_fit.render_artifact()
