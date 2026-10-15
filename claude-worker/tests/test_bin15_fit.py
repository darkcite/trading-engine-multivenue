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

import decimal
import pathlib
import re

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
    assert phi[-1] >= claude_worker.bin15_fit.PHI_LAST_MIN_1E6


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


# --- DISTX (2026-09-23): the Student-t price map and the hour table ------
#
# Every number below is a NON-research value (nu 2 / 3 / 4 / 1e7, s 1 or
# 0.3, round hour offsets). The fitted DISTX parameters are research, and
# research never enters git; the acceptance against the operator's draft
# is a vault one-shot.

#: 24 round offsets of both signs, the first and the last ON the fitter's
#: ln 2 bound (so the bound is inclusive, and the CLI spelling for a table
#: that starts negative is exercised).
_HOURS: tuple[int, ...] = (
    -claude_worker.bin15_fit.HOUR_LN_OFF_ABS_MAX_1E9,
    *(h * 10_000_000 for h in range(-11, 11)),
    claude_worker.bin15_fit.HOUR_LN_OFF_ABS_MAX_1E9,
)
_HOURS_ARG: str = ",".join(str(v) for v in _HOURS)


def test_the_floor_mirrors_the_grammar() -> None:
    """``PHI_LAST_MIN_1E6`` is ``core_config::bin15``'s, read off the Rust
    source. A fitter that let through a table the engine then refuses at
    boot would hand the operator a KeepAlive refusal loop."""
    src = (_ROOT / "crates" / "core-config" / "src" / "bin15.rs").read_text(encoding="utf-8")
    assert "PHI_LAST_MIN_1E6: u32 = 980_000" in src
    found = re.search(r"pub const PHI_LAST_MIN_1E6: u32 = ([0-9_]+);", src)
    assert found is not None
    assert int(found.group(1).replace("_", "")) == claude_worker.bin15_fit.PHI_LAST_MIN_1E6


def test_the_t_cdf_is_the_closed_form_at_two_degrees_of_freedom() -> None:
    """``T_2(x) = 1/2 + x / (2 sqrt(2 + x^2))`` at EVERY grid point.

    nu = 2 is the one the table builder refuses (its ``k`` needs a
    variance), so this holds the CDF function itself to the closed form.
    The grid straddles the continued fraction's swap at ``x = sqrt 1.5``.
    """
    two = decimal.Decimal(2)
    worst = decimal.Decimal(0)
    with decimal.localcontext() as ctx:
        ctx.prec = claude_worker.bin15_fit.PRECISION
        for i in range(claude_worker.bin15_fit.PHI_POINTS):
            x = decimal.Decimal(i) / 1_000
            want = decimal.Decimal(1) / 2 + x / (2 * (2 + x * x).sqrt())
            worst = max(worst, abs(claude_worker.bin15_fit.student_t_cdf(two, x) - want))
    assert worst < decimal.Decimal(10) ** -45, f"worst |T_2 - closed form| = {worst}"


def test_the_nu_4_table_is_its_closed_form_through_k() -> None:
    """At nu = 4 the CDF is ``1/2 + t (t^2 + 6) / (2 (t^2 + 4)^(3/2))``, so
    the TABLE is checked end to end on a stride of the grid: ``k =
    sqrt(nu / (nu - 2)) = sqrt 2`` at s = 1, then Phi's own rounding."""
    table = claude_worker.bin15_fit.student_t_table_1e6("4", "1")
    with decimal.localcontext() as ctx:
        ctx.prec = claude_worker.bin15_fit.PRECISION
        half = decimal.Decimal(1) / 2
        k = decimal.Decimal(2).sqrt()
        for i in range(0, claude_worker.bin15_fit.PHI_POINTS, 64):
            t = k * decimal.Decimal(i) / 1_000
            t2 = t * t
            scaled = (half + t * (t2 + 6) / (2 * (t2 + 4) * (t2 + 4).sqrt())) * 1_000_000
            whole = int(scaled)
            want = whole + 1 if scaled - whole >= half else whole
            assert table[i] == want, f"d = {i / 1000}: table {table[i]}, closed form {want}"


def test_a_fat_tailed_table_is_monotone_and_over_the_floor() -> None:
    table = claude_worker.bin15_fit.student_t_table_1e6("4", "1")
    assert len(table) == claude_worker.bin15_fit.PHI_POINTS
    assert table[0] == 500_000
    assert all(table[i + 1] >= table[i] for i in range(len(table) - 1))
    assert table[-1] >= claude_worker.bin15_fit.PHI_LAST_MIN_1E6
    # Fatter than Phi: less certain at the clamp than the Gaussian's 999_979.
    assert table[-1] < claude_worker.bin15_fit.phi_table_1e6()[-1]


def test_a_huge_nu_is_phi_to_one_unit_everywhere() -> None:
    """As nu grows the t becomes the normal: at nu = 1e7 (s = 1, so ``k``
    is 1 + 1e-7) every point is within one 1e-6 unit of Phi's table."""
    table = claude_worker.bin15_fit.student_t_table_1e6(10**7, 1)
    phi = claude_worker.bin15_fit.phi_table_1e6()
    worst = max(abs(a - b) for a, b in zip(table, phi, strict=True))
    assert worst <= 1, f"max |t - Phi| = {worst}"


@pytest.mark.parametrize(
    ("nu", "s"),
    [("2", "1"), ("1.5", "1"), ("0", "1"), ("4", "0"), ("4", "-1")],
)
def test_a_t_table_without_a_variance_or_a_scale_is_refused(nu: str, s: str) -> None:
    with pytest.raises(ValueError, match="must be"):
        claude_worker.bin15_fit.student_t_table_1e6(nu, s)


def test_a_float_parameter_is_refused_not_rounded() -> None:
    """A float has already rounded the fit; the table must not depend on
    which binary neighbour it picked."""
    with pytest.raises(TypeError):
        claude_worker.bin15_fit.student_t_table_1e6(4.0, "1")
    with pytest.raises(TypeError):
        claude_worker.bin15_fit.student_t_table_1e6("4", 1.0)
    with pytest.raises(ValueError, match="finite"):
        claude_worker.bin15_fit.student_t_table_1e6("NaN", "1")


def test_a_t_table_under_the_grammar_floor_is_refused() -> None:
    """nu = 3 at 0.3 of the scale ends near 0.94 at the clamp: a table the
    engine would refuse at boot is refused here first."""
    with pytest.raises(ValueError, match="under the grammar's floor"):
        claude_worker.bin15_fit.student_t_table_1e6("3", "0.3")


def test_validate_refuses_every_table_the_grammar_refuses() -> None:
    phi = claude_worker.bin15_fit.phi_table_1e6()
    claude_worker.bin15_fit.validate_phi_lut(phi)
    dip = list(phi)
    dip[100] = dip[99] - 1
    with pytest.raises(ValueError, match=r"not monotone at \[100\]"):
        claude_worker.bin15_fit.validate_phi_lut(dip)
    with pytest.raises(ValueError, match="points"):
        claude_worker.bin15_fit.validate_phi_lut(phi[:-1])
    with pytest.raises(ValueError, match=r"\[0\] = 499999"):
        claude_worker.bin15_fit.validate_phi_lut((499_999, *phi[1:]))
    with pytest.raises(ValueError, match="outside"):
        claude_worker.bin15_fit.validate_phi_lut((*phi[:-1], 1_000_001))
    with pytest.raises(ValueError, match="under the grammar's floor"):
        claude_worker.bin15_fit.validate_phi_lut(tuple(min(v, 979_999) for v in phi))


def test_phi_lut_refuses_a_parameter_it_would_ignore() -> None:
    with pytest.raises(ValueError, match="no nu or s"):
        claude_worker.bin15_fit.phi_lut_1e6("normal", phi_nu="4")
    with pytest.raises(ValueError, match="needs both"):
        claude_worker.bin15_fit.phi_lut_1e6("student-t", phi_nu="4")
    with pytest.raises(ValueError, match="one of"):
        claude_worker.bin15_fit.phi_lut_1e6("laplace")
    assert claude_worker.bin15_fit.phi_lut_1e6() == claude_worker.bin15_fit.phi_table_1e6()


def test_the_hour_table_is_one_line_right_after_the_scale() -> None:
    text = claude_worker.bin15_fit.render_artifact(hour_ln_off_1e9=_HOURS)
    lines = text.splitlines()
    at = lines.index(f"scale_1e9       = {claude_worker.bin15_fit.SCALE_1E9_DEFAULT}")
    assert lines[at + 1] == "hour_ln_off_1e9 = [" + ", ".join(str(v) for v in _HOURS) + "]"
    assert at + 2 == len(lines), "the hour line is the artifact's last line"
    keys = [ln.split("=", 1)[0].strip() for ln in lines if "=" in ln and ln[:1] != "#"]
    assert keys.count("hour_ln_off_1e9") == 1
    # The header stops saying there is no hour table, and says what it is.
    assert "is ABSENT on purpose" not in text
    assert "is an hour-of-day table" in text
    # Of the key lines, only the hour line is new.
    moved = set(lines) ^ set(claude_worker.bin15_fit.render_artifact().splitlines())
    assert {ln for ln in moved if ln[:1] != "#"} == {lines[at + 1]}


@pytest.mark.parametrize(
    "hours",
    [
        _HOURS[:-1],
        (*_HOURS, 0),
        (claude_worker.bin15_fit.HOUR_LN_OFF_ABS_MAX_1E9 + 1, *_HOURS[1:]),
        (*_HOURS[:-1], -claude_worker.bin15_fit.HOUR_LN_OFF_ABS_MAX_1E9 - 1),
    ],
)
def test_a_malformed_hour_table_is_refused(hours: tuple[int, ...]) -> None:
    with pytest.raises(ValueError, match="hour_ln_off_1e9"):
        claude_worker.bin15_fit.hour_table_1e9(hours)
    with pytest.raises(ValueError, match="hour_ln_off_1e9"):
        claude_worker.bin15_fit.render_tables(hour_ln_off_1e9=hours)


def test_an_hour_offset_is_an_integer_or_nothing() -> None:
    with pytest.raises(TypeError):
        claude_worker.bin15_fit.hour_table_1e9((1.5, *_HOURS[1:]))
    with pytest.raises(TypeError):
        claude_worker.bin15_fit.hour_table_1e9((True, *_HOURS[1:]))


def test_the_hour_bound_is_ln_2() -> None:
    with decimal.localcontext() as ctx:
        ctx.prec = 40
        ln2_1e9 = decimal.Decimal(2).ln() * 1_000_000_000
    assert claude_worker.bin15_fit.HOUR_LN_OFF_ABS_MAX_1E9 == int(
        ln2_1e9.to_integral_value(rounding=decimal.ROUND_HALF_UP)
    )


def test_the_tables_lane_with_the_fit_flags_emits_exactly_the_expected_lines(
    tmp_path: pathlib.Path,
) -> None:
    out = tmp_path / "tables.toml"
    argv = ["tables", "--out", str(out), "--phi", "student-t", "--phi-nu", "4", "--phi-s", "1"]
    assert claude_worker.bin15_fit.main([*argv, f"--hour-ln-off-1e9={_HOURS_ARG}"]) == 0
    text = out.read_text(encoding="utf-8")
    assert text == claude_worker.bin15_fit.render_tables(
        phi="student-t", phi_nu="4", phi_s="1", hour_ln_off_1e9=_HOURS
    )
    lines = text.splitlines()
    assert [ln.split("=", 1)[0].strip() for ln in lines] == [
        "phi_lut",
        "recal_early",
        "recal_mid",
        "recal_late",
        "scale_1e9",
        "hour_ln_off_1e9",
    ]
    table = claude_worker.bin15_fit.student_t_table_1e6("4", "1")
    assert lines[0] == "phi_lut = [" + ", ".join(str(v) for v in table) + "]"
    assert not list(tmp_path.glob("*.tmp")), "the temp file must be renamed away"


def test_the_tables_lane_prints_an_hour_table_under_phi(
    capsys: pytest.CaptureFixture[str],
) -> None:
    hours = tuple(abs(v) for v in _HOURS)
    arg = ",".join(str(v) for v in hours)
    assert claude_worker.bin15_fit.main(["tables", "--hour-ln-off-1e9", arg]) == 0
    printed = capsys.readouterr().out
    assert printed == claude_worker.bin15_fit.render_tables(hour_ln_off_1e9=hours)
    assert printed.startswith(claude_worker.bin15_fit.render_tables())


@pytest.mark.parametrize(
    "extra",
    [
        ["--phi", "student-t"],
        ["--phi", "student-t", "--phi-nu", "4"],
        ["--phi-nu", "4", "--phi-s", "1"],
        ["--phi", "student-t", "--phi-nu", "four", "--phi-s", "1"],
        ["--phi", "student-t", "--phi-nu", "NaN", "--phi-s", "1"],
        ["--phi", "laplace"],
        ["--hour-ln-off-1e9", "1,2,3"],
        ["--hour-ln-off-1e9", ",".join(["693147182"] * 24)],
        ["--hour-ln-off-1e9", ",".join(["x"] * 24)],
    ],
)
def test_the_cli_refuses_a_malformed_fit(extra: list[str], tmp_path: pathlib.Path) -> None:
    """Each is an argparse refusal (exit 2) on both lanes, and a refused
    fit writes nothing."""
    for lane in (["tables"], ["artifact", "--out", str(tmp_path / "bin15.toml")]):
        with pytest.raises(SystemExit) as exc:
            claude_worker.bin15_fit.main([*lane, *extra])
        assert exc.value.code == 2
    assert not list(tmp_path.iterdir())


def test_the_artifact_lane_writes_a_t_artifact_atomically(tmp_path: pathlib.Path) -> None:
    out = tmp_path / "bin15.toml"
    argv = ["artifact", "--out", str(out), "--phi", "student-t", "--phi-nu", "4", "--phi-s", "1"]
    assert claude_worker.bin15_fit.main([*argv, f"--hour-ln-off-1e9={_HOURS_ARG}"]) == 0
    text = out.read_text(encoding="utf-8")
    assert text == claude_worker.bin15_fit.render_artifact(
        phi="student-t", phi_nu="4", phi_s="1", hour_ln_off_1e9=_HOURS
    )
    assert not list(tmp_path.glob("*.tmp")), "the temp file must be renamed away"
    assert "# nu = 4, s = 1." in text
    assert "# Phi is computed exactly" not in text
    # Of the KEY lines, only the price map and the hour table moved.
    moved = set(text.splitlines()) ^ set(claude_worker.bin15_fit.render_artifact().splitlines())
    assert {ln.split("=", 1)[0].strip() for ln in moved if ln[:1] != "#"} == {
        "phi_lut",
        "hour_ln_off_1e9",
    }


def test_the_summary_names_the_map_and_its_ends(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    out = tmp_path / "tables.toml"
    argv = ["tables", "--out", str(out), "--phi", "student-t", "--phi-nu", "4", "--phi-s", "1"]
    assert claude_worker.bin15_fit.main(argv) == 0
    err = capsys.readouterr().err
    table = claude_worker.bin15_fit.student_t_table_1e6("4", "1")
    assert "(phi student-t nu 4 s 1 4097 pts" in err
    assert f"[{table[0]}..{table[-1]}]" in err
    assert "hours none" in err
