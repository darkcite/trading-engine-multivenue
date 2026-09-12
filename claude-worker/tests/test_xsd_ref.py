# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The xsd mirror: its integer primitives, and the Rust ↔ Python parity fixture."""

import math
import pathlib
import random

import pytest

import claude_worker.xsd_ref as xsd_ref

FIXTURES: pathlib.Path = pathlib.Path(__file__).parent / "fixtures" / "xsd"


def test_ln1e9_within_one_unit_of_the_float_log() -> None:
    fixed: list[int] = [1, 2, 3, 7, 10, 999_999, 1_000_000, 1_000_001, 65_000_000_000,
                        200_000_000_000, 10**12, 123_456_789_012_345, 1 << 62, (1 << 63) - 1]
    rng: random.Random = random.Random(7)
    for _ in range(20_000):
        fixed.append(max(1, int(2 ** rng.uniform(0.0, 63.0))))
    for v in fixed:
        ref: int = round(math.log(v) * 1e9)
        assert abs(xsd_ref.ln1e9(v) - ref) <= 1, v
    assert xsd_ref.ln1e9(1) == 0
    assert xsd_ref.ln1e9(2) == 693_147_181
    assert xsd_ref.ln1e9(1 << 40) == 27_725_887_222
    with pytest.raises(ValueError):
        xsd_ref.ln1e9(0)


def test_spread_and_min_count_laws() -> None:
    assert xsd_ref.spread_1e9(10, 3, 500_000_000) == 9
    assert xsd_ref.spread_1e9(10, -3, 500_000_000) == 12
    assert xsd_ref.min_count(720) == 180
    assert xsd_ref.min_count(100) == 30
    assert xsd_ref.min_count(124) == 31


def test_z_matches_the_closed_form_and_refuses_the_mask() -> None:
    total: int = sum(range(30))
    total2: int = sum(i * i for i in range(30))
    assert xsd_ref.z_1e9(total, total2, 30, True, 29, 100) == (29 - 14) * 10**9 // 9
    assert xsd_ref.z_1e9(total, total2, 30, False, 29, 100) is None
    assert xsd_ref.z_1e9(1, 1, 1, True, 1, 100) is None
    assert xsd_ref.z_1e9(5 * 40, 25 * 40, 40, True, 5, 100) is None


def _params(**over: int) -> xsd_ref.Params:
    base: dict[str, int] = {
        "z_window_h": 96, "z_enter_1e9": 3 * 10**9, "z_exit_1e9": 0, "z_stop_1e9": 5 * 10**9,
        "consensus": 1, "grid_n": 1, "grid_step_1e9": 5 * 10**8, "max_hold_h": 240,
        "cooldown_h": 1, "ttl_ns": 300 * 10**9, "position_usd_1e6": 10**9,
        "max_positions": 82, "max_gross_usd_1e6": xsd_ref.CAP_TABLE_1E6, "direction": 1,
        "slip_1e9": 0,
    }
    base.update(over)
    return xsd_ref.Params(**base)


def test_validate_params_refuses_the_same_shapes_as_the_crate() -> None:
    xsd_ref.validate_params(_params())
    for bad in ({"z_window_h": 39}, {"z_window_h": 2161}, {"z_stop_1e9": 3 * 10**9},
                {"z_enter_1e9": 0}, {"consensus": 4}, {"grid_n": 9},
                {"grid_n": 3, "grid_step_1e9": 0}, {"max_hold_h": 0}, {"ttl_ns": 0},
                {"position_usd_1e6": xsd_ref.CAP_LEG_1E6 + 1}, {"max_positions": 0},
                {"direction": 0}, {"slip_1e9": -1}):
        with pytest.raises(ValueError):
            xsd_ref.validate_params(_params(**bad))
    with pytest.raises(ValueError):
        xsd_ref.XsdRef(_params(), [])
    with pytest.raises(ValueError):
        xsd_ref.XsdRef(_params(), [(1, 1, 10**9)])
    with pytest.raises(ValueError):
        xsd_ref.XsdRef(_params(), [(1, 2, 0)])
    with pytest.raises(ValueError):
        xsd_ref.XsdRef(_params(), [(1, 2, 1), (1, 2, 2)])
    with pytest.raises(ValueError):
        xsd_ref.XsdRef(_params(), [(1, 2, 1), (1, 3, 1), (1, 4, 1), (1, 5, 1)])


def test_seed_law_and_a_dislocation_enters_then_reverts() -> None:
    hour_ns: int = xsd_ref.HOUR_NS
    a: int = (1 << 24) | 515
    b: int = (1 << 24) | 516
    ref: xsd_ref.XsdRef = xsd_ref.XsdRef(_params(z_stop_1e9=1000 * 10**9), [(a, b, 10**9)])
    hour0: int = 496_992
    for k, h in enumerate(range(hour0 - 95, hour0)):
        assert ref.seed_close(a, h, 100_000_000 if k % 2 == 0 else 100_200_000)
        assert ref.seed_close(b, h, 100_000_000)
    assert not ref.seed_close(a, hour0 - 1, 101_000_000), "a filled bucket is never overwritten"
    assert not ref.seed_close(a, hour0 - 1 - xsd_ref.XSD_RING_H, 100_000_000), "older than the ring"
    assert not ref.seed_close(999, hour0 - 1, 100_000_000), "unknown sym"
    assert not ref.seed_close(a, hour0 - 3, 1), "a micro-dollar close is absent"
    assert ref.counters["seed_rows"] == 190 and ref.counters["seed_dropped"] == 4
    ref.on_timer(hour0 * hour_ns + 1)
    assert ref.on_tick(a, hour0 * hour_ns + 10, 105_000_000, 105_000_000) is None
    assert ref.on_tick(b, hour0 * hour_ns + 11, 100_000_000, 100_000_000) is None
    ref.on_timer((hour0 + 1) * hour_ns + 1)
    tg = ref.targets[0]
    assert tg.state == xsd_ref.ST_PENDING_ENTER and tg.d == 1 and tg.side == xsd_ref.SIDE_LONG
    assert tg.zbar_1e9 > 3 * 10**9
    # Stale ticks price nothing; the first fresh one buys at the ask.
    assert ref.on_tick(a, (hour0 + 1) * hour_ns + 5, 105_100_000, 105_100_000, stale=True) is None
    order = ref.on_tick(a, (hour0 + 1) * hour_ns + 6, 105_100_000, 105_100_000)
    assert order is not None and order.side == xsd_ref.SIDE_BID and order.px_1e6 == 105_100_000
    assert order.qty_1e6 == xsd_ref.qty_for_notional_1e6(10**9, 105_100_000)
    assert tg.state == xsd_ref.ST_ENTERED and ref.n_positions == 1
    # Collapse ⇒ revert exit at the bid, the whole stack.
    ref.on_tick(a, (hour0 + 1) * hour_ns + 7, 100_000_000, 100_000_000)
    ref.on_tick(b, (hour0 + 1) * hour_ns + 8, 100_000_000, 100_000_000)
    ref.on_timer((hour0 + 2) * hour_ns + 1)
    assert tg.intent == xsd_ref.INTENT_EXIT and tg.exit_reason == xsd_ref.EXIT_REVERT
    x = ref.on_tick(a, (hour0 + 2) * hour_ns + 5, 100_000_000, 100_000_000)
    assert x is not None and x.side == xsd_ref.SIDE_ASK and x.qty_1e6 == order.qty_1e6
    assert tg.state == xsd_ref.ST_FLAT and ref.counters["exits_revert"] == 1
    assert ref.n_positions == 0 and ref.open_notional_total_1e6 == 0


def test_parity_fixture_matches_the_engine() -> None:
    inp: pathlib.Path = FIXTURES / "parity-1.input.tsv"
    exp: pathlib.Path = FIXTURES / "parity-1.expected.tsv"
    assert exp.exists(), "regenerate with XSD_PARITY_WRITE=1 cargo nextest run -p strategy-xsd --test parity"
    fx: xsd_ref.Fixture = xsd_ref.parse_fixture(inp.read_text())
    got: list[str] = xsd_ref.run_fixture(fx)
    want: list[str] = [l for l in exp.read_text().splitlines() if l and not l.startswith("#")]
    assert len(got) == len(want)
    for i, (g, w) in enumerate(zip(got, want)):
        assert g == w, "line %d differs" % (i + 1)


def test_parity_fixture_covers_the_decision_table() -> None:
    fx: xsd_ref.Fixture = xsd_ref.parse_fixture((FIXTURES / "parity-1.input.tsv").read_text())
    lines: list[str] = xsd_ref.run_fixture(fx)
    counts: dict[str, int] = dict(zip(xsd_ref.COUNTER_NAMES, (int(x) for x in lines[-1].split()[1:])))
    assert counts["entries"] >= 8
    assert counts["adds"] >= 2
    assert counts["exits_revert"] >= 3
    assert counts["exits_stop"] >= 1
    assert counts["exits_maxhold"] >= 1
    assert counts["intents_carried"] >= 1
    assert counts["holds_absent"] >= 1
    sides: set[str] = {l.split()[3] for l in lines if l.startswith("O ")}
    assert sides == {"0", "1"}
