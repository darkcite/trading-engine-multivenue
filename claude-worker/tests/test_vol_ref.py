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

import pathlib

import pytest

import claude_worker.vol_ref

_DIR: pathlib.Path = pathlib.Path(__file__).resolve().parent / "fixtures" / "vol"
_FIXTURES: tuple[str, ...] = ("parity-1",)


def _opt(v: int | None) -> str:
    return "-" if v is None else str(v)


def _replay(name: str) -> list[str]:
    """Run the op tape, returning one row per ``Q``."""
    src = (_DIR / f"{name}.input.tsv").read_text(encoding="utf-8")
    tau_ns = 0
    theta_1e9 = 0
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
            engine.arm_hold(tau_ns, int(f[1]))
        elif op == "S":
            engine.observe_settlement(int(f[1]))
        elif op == "Q":
            fit = engine.fit()
            a = None if fit is None else fit[0]
            b = None if fit is None else fit[1]
            bounds = engine.bounds(tau_ns, theta_1e9)
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
    assert claude_worker.vol_ref.tenor_of(claude_worker.vol_ref.TAU_4H_NS) is not None
    assert claude_worker.vol_ref.tenor_of(claude_worker.vol_ref.TAU_8H_NS) is not None
    assert claude_worker.vol_ref.tenor_of(43_200_000_000_000) is None
    assert claude_worker.vol_ref.tenor_of(86_400_000_000_000) is None
