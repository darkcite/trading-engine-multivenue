# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""vol_ref.py — the Python half of the Rust ↔ Python forecast parity (VRP V4).

``tests/fixtures/vol/parity-<n>.{input,expected}.tsv`` are the SAME files
``crates/core-vol/tests/parity.rs`` consumes; the expected file is written
by the Rust law (``CORE_VOL_PARITY_WRITE=1``) and asserted here row by
row, so neither implementation can drift without one suite going red.

This matters more than an ordinary parity test. The boot seed the engine
fits from (V5) is cut by this module; the pairs the engine forms
afterwards continue the same series. A one-unit disagreement anywhere and
the line the engine trades on is not the line the research measured, and
nothing else in the lane would notice.

Convention: full ``import x`` only. No ``from x import y``.
"""

import math
import pathlib

import pytest

import claude_worker.vol_ref

_DIR: pathlib.Path = pathlib.Path(__file__).resolve().parent / "fixtures" / "vol"
#: ``parity-1`` is the VRP lane's 8 h tape; ``parity-15m`` is BIN15's,
#: added by O4a to cover the fourth HAR window, the IV-optional arm and
#: ``sigma_hat_1e9``. ``parity-1`` was NOT regenerated when the 15-minute
#: term landed -- that file staying green is the bit-identity proof.
_FIXTURES: tuple[str, ...] = ("parity-1", "parity-15m")


def _opt(v: int | None) -> str:
    return "-" if v is None else str(v)


def _replay(name: str) -> list[str]:
    """Run the op tape, returning one row per ``Q`` or ``G``."""
    src = (_DIR / f"{name}.input.tsv").read_text(encoding="utf-8")
    tau_ns = 0
    theta_1e9 = 0
    # R3: the regime's log-vol intercept in force, sticky until the next
    # ``O``. Zero for every row the fixture wrote before P4.1.
    off_1e9 = 0
    engine = claude_worker.vol_ref.VolEngine()
    out: list[str] = []
    row = 0
    for line in src.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        f = stripped.split("\t")
        if f[0].startswith("tau_ns="):
            tau_ns = int(f[0][len("tau_ns=") :])
            theta_1e9 = int(f[1][len("theta_1e9=") :])
            continue
        op = f[0]
        if op == "C":
            engine.on_minute_close(int(f[1]))
        elif op == "P":
            engine.seed_pair(int(f[1]), int(f[2]))
        elif op == "A":
            engine.arm_hold_with_offset(tau_ns, int(f[1]), off_1e9)
        elif op == "O":
            off_1e9 = int(f[1])
        elif op == "S":
            engine.observe_settlement(int(f[1]))
        elif op == "R":
            # F1: settle from the engine's OWN realised window -- the law
            # the member uses live. 0 when the window is absent, which
            # observe_settlement ignores by contract.
            engine.observe_settlement(engine.realised_since_arm_1e9() or 0)
        elif op == "D":
            engine.disarm()
        elif op == "G":
            # BIN15 O4a: sigma_hat over tau, the raw per-tau vol the
            # binary pricer consumes. Its own row shape, so a tape
            # without ``G`` -- parity-1 -- keeps the rows already pinned.
            out.append(
                "\t".join(
                    [
                        str(row),
                        "G",
                        _opt(engine.sigma_hat_1e9(tau_ns)),
                        _opt(engine.ln_sigma_hat_1e9(tau_ns)),
                    ]
                )
            )
            row += 1
        elif op == "Q":
            fit = engine.fit()
            a = None if fit is None else fit[0]
            b = None if fit is None else fit[1]
            bounds = engine.bounds_with_offset(tau_ns, theta_1e9, off_1e9)
            lo = None if bounds is None else bounds[0]
            hi = None if bounds is None else bounds[1]
            q = engine.qlike_counters()
            out.append(
                "\t".join(
                    [
                        str(row),
                        str(engine.minutes),
                        str(len(engine.pair_x_1e9)),
                        _opt(engine.har_1e9(tau_ns)),
                        _opt(engine.x_1e9(tau_ns)),
                        _opt(a),
                        _opt(b),
                        _opt(engine.ln_sigma_hat_1e9(tau_ns)),
                        _opt(lo),
                        _opt(hi),
                        str(q[0]),
                        str(q[1]),
                        str(q[2]),
                        "1" if q[3] else "0",
                        "1" if engine.armed else "0",
                        _opt(engine.realised_since_arm_1e9()),
                    ]
                )
            )
            row += 1
        else:  # pragma: no cover - the fixture is ours
            raise AssertionError(f"{name}: unknown op {op!r}")
    assert out, f"{name}: the tape emitted no rows"
    return out


def _expected(name: str) -> list[str]:
    text = (_DIR / f"{name}.expected.tsv").read_text(encoding="utf-8")
    return [
        line.strip()
        for line in text.splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]


@pytest.mark.parametrize("name", _FIXTURES)
def test_forecast_law_matches_the_shared_fixture(name: str) -> None:
    got = _replay(name)
    want = _expected(name)
    assert len(got) == len(want), f"{name}: row count drifted"
    for g, w in zip(got, want, strict=True):
        assert g == w, f"{name}: the forecast law drifted from the Rust"


def test_the_settlement_y_is_the_holds_own_returns_not_the_forecast() -> None:
    """F1: ``y`` is realised vol over the hold, never the HAR forecast.

    Live, the member paired ``x = ln(HAR at entry)`` with
    ``y = ln(HAR at expiry)`` -- two smoothed values sharing 16 of 24
    hours of window. The fitted slope drifts toward 1 as those pairs
    displace the seed's, and kill criterion 3 scores a forecast against
    itself: the host read ``qlike_har`` 0.000362 against ``qlike_iv``
    0.1255. This pins the quantity the seed cutter forms.
    """
    engine = claude_worker.vol_ref.VolEngine()
    px = 79_000_000_000
    for i in range(1_600):
        px += 3_000_000 if i % 3 else -5_000_000
        engine.on_minute_close(px)
    assert engine.realised_since_arm_1e9() is None, "nothing armed"
    assert engine.arm_hold(claude_worker.vol_ref.TAU_8H_NS, 600_000_000) is not None
    assert engine.realised_since_arm_1e9() is None, "the hold has not run"

    tau_min = claude_worker.vol_ref.tenor_of(claude_worker.vol_ref.TAU_8H_NS)[0]
    hold: list[int] = [px]  # the close the hold's first return is formed against
    for i in range(tau_min):
        px += 4_000_000 if i % 2 else -3_000_000
        hold.append(px)
        engine.on_minute_close(px)

    # Independently, from the CLOSES -- exactly what
    # vrp_seed.realised_rv_1e9 computes over candles.db.
    acc = 0
    prev = hold[0]
    for close in hold[1:]:
        r = claude_worker.vol_ref.ret_bps_1e9(prev, close)
        acc += r * r
        prev = close
    want = claude_worker.vol_ref.isqrt_i64(acc)
    assert want > 0
    assert engine.realised_since_arm_1e9() == want
    assert engine.realised_since_arm_1e9() != engine.har_1e9(
        claude_worker.vol_ref.TAU_8H_NS
    ), "y must not be the forecast"

    # F4: disarming drops the hold without forming a pair.
    engine.disarm()
    assert not engine.armed
    assert engine.realised_since_arm_1e9() is None
    engine.observe_settlement(want)
    assert engine.pair_x_1e9 == []


def test_the_constant_tables_are_the_same_two_tables() -> None:
    """The tables are the one place a float touches this law.

    ``crates/core-vol/src/fx.rs`` carries them as literals and re-derives
    them from ``f64`` in its own test; this module builds them at import
    from the same expression. Pinning the endpoints and the length here
    catches a truncated or reordered table before the fixture does, with
    a message that says which table.
    """
    log2 = claude_worker.vol_ref.LOG2_TAB
    exp2 = claude_worker.vol_ref.EXP2_TAB
    assert len(log2) == 257 and len(exp2) == 257
    assert log2[0] == 0 and log2[256] == 1_000_000_000
    assert exp2[0] == 1 << 32 and exp2[256] == 1 << 33
    # Strictly increasing: an interpolation over a non-monotone table
    # would produce a non-monotone logarithm.
    assert all(log2[i] < log2[i + 1] for i in range(256))
    assert all(exp2[i] < exp2[i + 1] for i in range(256))


def test_the_lookahead_guard_holds() -> None:
    """A settlement with nothing armed can never form a pair.

    The pair is ``(x formed BEFORE the hold, y realised over it)``. If a
    settlement could form its own ``x``, that ``x`` would be computed
    from minutes inside the hold — a lookahead, and the exact defect that
    makes an offline backtest of this strategy look profitable when it is
    not.
    """
    engine = claude_worker.vol_ref.VolEngine()
    px = 79_000_000_000
    for i in range(1_500):
        px += 3_000_000 if i % 3 else -5_000_000
        engine.on_minute_close(px)
    assert engine.har_1e9(claude_worker.vol_ref.TAU_8H_NS) is not None
    assert not engine.armed
    engine.observe_settlement(1_200_000_000_000)
    assert len(engine.pair_x_1e9) == 0

    x = engine.arm_hold(claude_worker.vol_ref.TAU_8H_NS, 600_000_000)
    assert x is not None
    engine.observe_settlement(1_200_000_000_000)
    assert engine.pair_x_1e9 == [x]
    assert not engine.armed


def test_only_the_measured_tenors_exist() -> None:
    """12 h and 24 h are not tradeable cells; they are absent, not gated."""
    ref = claude_worker.vol_ref
    assert ref.tenor_of(ref.TAU_15M_NS) is not None
    assert ref.tenor_of(ref.TAU_4H_NS) is not None
    assert ref.tenor_of(ref.TAU_8H_NS) is not None
    assert ref.tenor_of(43_200_000_000_000) is None
    assert ref.tenor_of(86_400_000_000_000) is None
    assert ref.tenor_of(1_800_000_000_000) is None, "30 m is not a tenor"
    assert ref.tenor_of(0) is None
    # The window split IS the bit-identity contract: 4 h and 8 h skip the
    # 15-minute term, 15 m folds it.
    assert ref.tenor_of(ref.TAU_15M_NS) == (15, ref.ANNUALISE_15M_1E9, 0)
    assert ref.tenor_of(ref.TAU_4H_NS) == (240, ref.ANNUALISE_4H_1E9, 1)
    assert ref.tenor_of(ref.TAU_8H_NS) == (480, ref.ANNUALISE_8H_1E9, 1)
    # Every annualiser comes out of the same 525 960-minute year.
    for tau_ns, (tau_min, annualise, _first) in ref.TENORS.items():
        assert tau_ns // 60_000_000_000 == tau_min
        repro = math.isqrt(525_960 * 10**18 // tau_min)
        assert abs(annualise - repro) <= 1, (tau_min, annualise, repro)


def test_the_15m_window_is_additive_for_the_longer_tenors() -> None:
    """BIN15 O4a. The fourth HAR window must not move a 4 h or 8 h number.

    The engine now carries a 15-minute rolling sum, but ``first_window``
    skips it for the longer tenors and the divisor stays 3 -- so folding
    the three long windows by hand, exactly as the pre-O4a body did, has
    to reproduce ``har_1e9`` to the unit. And 15 m has to differ, or the
    new window is doing nothing.
    """
    ref = claude_worker.vol_ref
    engine = ref.VolEngine()
    px = 79_000_000_000
    for i in range(1_600):
        px += 3_000_000 if i % 3 else -5_000_000
        engine.on_minute_close(px)
    long_mean_sq = 0
    for w in range(1, len(ref.HAR_WINDOWS)):
        rv = ref.isqrt_i64(engine.sum_sq[w])
        long_mean_sq += rv * rv // ref.HAR_WINDOWS[w]
    for tau_ns, tau_min in ((ref.TAU_4H_NS, 240), (ref.TAU_8H_NS, 480)):
        want = ref.isqrt_i64(long_mean_sq // 3 * tau_min)
        assert engine.har_1e9(tau_ns) == want, tau_min
    all_mean_sq = long_mean_sq
    rv15 = ref.isqrt_i64(engine.sum_sq[0])
    all_mean_sq += rv15 * rv15 // ref.HAR_WINDOWS[0]
    assert engine.har_1e9(ref.TAU_15M_NS) == ref.isqrt_i64(all_mean_sq // 4 * 15)
    assert all_mean_sq != long_mean_sq, "the 15-minute term must carry variance"
    assert 0 < engine.sum_sq[0] < engine.sum_sq[1]


def test_an_iv_less_hold_forms_a_pair_and_no_qlike_row() -> None:
    """BIN15 O4a pins behaviour that already held on both sides.

    A HIP-4 outcome market has no implied vol, so ``mark_iv_1e9 <= 0`` is
    the normal case: the hold still forms its pair and refits, and scores
    no QLIKE row -- scoring a zero would make the HAR look infinitely
    better than an IV nobody quoted.
    """
    ref = claude_worker.vol_ref
    engine = ref.VolEngine()
    px = 79_000_000_000
    for i in range(1_500):
        px += 3_000_000 if i % 3 else -5_000_000
        engine.on_minute_close(px)
    for i in range(ref.MIN_PAIRS):
        engine.seed_pair(2_000_000_000 + i * 1_000_000, 1_900_000_000 + i * 900_000)
    assert engine.fit() is not None
    before = len(engine.pair_x_1e9)
    assert engine.arm_hold(ref.TAU_15M_NS, 0) is not None
    for i in range(15):
        px += 4_000_000 if i % 2 else -2_000_000
        engine.on_minute_close(px)
    engine.observe_settlement(engine.realised_since_arm_1e9() or 0)
    assert len(engine.pair_x_1e9) == before + 1, "the pair IS formed"
    assert engine.qlike_counters()[0] == 0, "and nothing is scored"
    assert engine.arm_hold(ref.TAU_15M_NS, 500_000_000) is not None
    for i in range(15):
        px += 4_000_000 if i % 2 else -2_000_000
        engine.on_minute_close(px)
    engine.observe_settlement(engine.realised_since_arm_1e9() or 0)
    assert engine.qlike_counters()[0] == 1, "an IV-bearing hold scores one row"


def test_sigma_hat_is_the_exponential_of_the_log_forecast() -> None:
    """BIN15 O4a. No annualiser, no band -- just ``exp(ln sigma_hat)``."""
    ref = claude_worker.vol_ref
    engine = ref.VolEngine()
    assert engine.sigma_hat_1e9(ref.TAU_15M_NS) is None, "cold engine"
    px = 79_000_000_000
    for i in range(1_500):
        px += 3_000_000 if i % 3 else -5_000_000
        engine.on_minute_close(px)
    assert engine.har_1e9(ref.TAU_15M_NS) is not None
    assert engine.sigma_hat_1e9(ref.TAU_15M_NS) is None, "unfitted engine holds"
    for i in range(ref.MIN_PAIRS):
        engine.seed_pair(2_000_000_000 + i * 1_000_000, 1_900_000_000 + i * 900_000)
    ln = engine.ln_sigma_hat_1e9(ref.TAU_15M_NS)
    assert ln is not None
    assert engine.sigma_hat_1e9(ref.TAU_15M_NS) == ref.exp_1e9(ln)
    assert engine.sigma_hat_1e9(4_000_000_000) is None, "an untraded tau is None"
