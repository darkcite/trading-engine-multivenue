# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""monitor.py — the §8.3 pure substrate (design §12 monitor rows):
threshold arithmetic both arms, run-span reading, trailing-window
selection + floor math, the active-artifact copy (report-clobber
protection), the window dir, and the registry accessor.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import pytest

import claude_worker.backtest
import claude_worker.monitor
import claude_worker.state
import tests.craft

_H_NS = 3_600_000_000_000  # one hour


def _harness(net: float, dd: float) -> claude_worker.backtest.HarnessReport:
    return claude_worker.backtest.HarnessReport(
        ruleset_hash="ab" * 32,
        split="0/100",
        oos_net_pnl_usd=net,
        oos_trades=10,
        oos_trading_days=1,
        oos_max_drawdown_usd=dd,
        max_order_notional_usd=50.0,
        max_symbol_notional_usd=90.0,
        max_total_notional_usd=90.0,
    )


# ---- §8.3 threshold arithmetic (both arms, boundary-exact) ---------------


@pytest.mark.parametrize(
    ("net", "dd", "triggered", "net_arm", "dd_arm"),
    [
        (-100.0, 0.0, True, True, False),  # net boundary INCLUSIVE
        (-99.999, 0.0, False, False, False),
        (-1000.0, 0.0, True, True, False),
        (5.0, 200.0, True, False, True),  # dd boundary INCLUSIVE
        (5.0, 199.999, False, False, False),
        (5.0, 500.0, True, False, True),
        (-150.0, 250.0, True, True, True),  # both arms
        (0.0, 0.0, False, False, False),
    ],
)
def test_breach_both_arms_boundary_exact(
    net: float, dd: float, triggered: bool, net_arm: bool, dd_arm: bool
) -> None:
    hit, metrics = claude_worker.monitor.breach(_harness(net, dd))
    assert hit is triggered
    assert metrics["net_trigger"] is net_arm
    assert metrics["drawdown_trigger"] is dd_arm
    assert metrics["net_pnl_usd"] == net
    assert metrics["max_drawdown_usd"] == dd


def test_threshold_constants_mirror_risk_policy() -> None:
    # −$100 = ½ the $200/day realized-loss kill line; dd cap = the line.
    assert claude_worker.monitor.MONITOR_NET_PNL_TRIGGER_USD == -100.0
    assert claude_worker.monitor.MONITOR_DRAWDOWN_TRIGGER_USD == 200.0
    assert claude_worker.monitor.MONITOR_WINDOW_NS == 24 * _H_NS
    assert claude_worker.monitor.MONITOR_FLOOR_NS == 6 * _H_NS
    assert claude_worker.monitor.MONITOR_SPLIT == "0/100"


# ---- run spans (O(1) per file) -------------------------------------------


def test_read_run_spans_durations_and_order(tmp_path: pathlib.Path) -> None:
    replay = tmp_path / "replay"
    tests.craft.write_run(replay, 1_000, [100, 500, 7_200_000_000_100])  # 2 h span
    tests.craft.write_run(replay, 2_000, [])  # tickless run
    run3 = tests.craft.write_run(replay, 3_000, [50, 3_600_000_000_050])  # 1 h
    # A second venue file widens run-3's span (shared clock, §3.2).
    tests.craft.write_ticks(run3 / "bn-ticks.pmlr", [10, 1_800_000_000_010], 3_000, sym=7, venue=1)
    (replay / "not-a-run").mkdir()

    spans = claude_worker.monitor.read_run_spans(replay)
    assert [s.epoch_ns for s in spans] == [1_000, 2_000, 3_000], "oldest first"
    assert spans[0].duration_ns == 7_200_000_000_000
    assert spans[1].duration_ns == 0
    assert spans[2].duration_ns == 3_600_000_000_040  # min first (10) .. max last
    assert spans[2].end_ns == 3_000 + 3_600_000_000_040


def test_read_run_spans_tolerates_torn_and_foreign_files(tmp_path: pathlib.Path) -> None:
    replay = tmp_path / "replay"
    run_dir = tests.craft.write_run(replay, 500, [0, _H_NS])
    (run_dir / "zz-ticks.pmlr").write_bytes(b"garbage")  # unreadable: skipped
    spans = claude_worker.monitor.read_run_spans(replay)
    assert len(spans) == 1 and spans[0].duration_ns == _H_NS


# ---- trailing-window selection + floor -----------------------------------


def _span(epoch_h: int, dur_h: int) -> claude_worker.monitor.RunSpan:
    return claude_worker.monitor.RunSpan(
        path=pathlib.Path(f"run-{epoch_h * _H_NS}"),
        epoch_ns=epoch_h * _H_NS,
        duration_ns=dur_h * _H_NS,
    )


def test_select_window_empty_is_none() -> None:
    assert claude_worker.monitor.select_window([]) is None


def test_select_window_run_granular_straddler_and_exclusion() -> None:
    # Capture: run A [0, 10h], run B [30h, 40h], run C [44h, 50h].
    spans = [_span(0, 10), _span(30, 10), _span(44, 6)]
    sel = claude_worker.monitor.select_window(spans)
    assert sel is not None
    assert sel.capture_end_ns == 50 * _H_NS
    assert sel.window_start_ns == 26 * _H_NS
    # A ends 10h <= 26h: excluded. B straddles: included WHOLE, but only
    # its in-window part [30h..40h] counts (fully inside). C fully in.
    assert [s.epoch_ns for s in sel.runs] == [30 * _H_NS, 44 * _H_NS]
    assert sel.coverage_ns == (10 + 6) * _H_NS
    assert sel.is_full_root is False
    assert sel.total_runs == 3


def test_select_window_straddler_coverage_clips_at_window_start() -> None:
    # One run [0h, 30h]: straddles start (6h..30h window) — coverage 24h.
    sel = claude_worker.monitor.select_window([_span(0, 30)])
    assert sel is not None
    assert sel.window_start_ns == 6 * _H_NS
    assert sel.coverage_ns == 24 * _H_NS
    assert sel.is_full_root is True


def test_select_window_floor_arithmetic() -> None:
    # 5h of capture in-window: below the 6h floor.
    sel = claude_worker.monitor.select_window([_span(0, 5)])
    assert sel is not None
    assert sel.coverage_ns == 5 * _H_NS
    assert sel.coverage_ns < claude_worker.monitor.MONITOR_FLOOR_NS
    # Exactly 6h: at the floor — NOT below (monitor proceeds).
    sel6 = claude_worker.monitor.select_window([_span(0, 6)])
    assert sel6 is not None and sel6.coverage_ns == claude_worker.monitor.MONITOR_FLOOR_NS


def test_select_window_tickless_runs_contribute_nothing() -> None:
    spans = [_span(0, 0), _span(1, 8)]
    sel = claude_worker.monitor.select_window(spans)
    assert sel is not None
    assert [s.epoch_ns for s in sel.runs] == [1 * _H_NS]
    assert sel.coverage_ns == 8 * _H_NS


# ---- scratch artifacts (report-clobber protection) + window dir ----------


def test_stage_active_copy_bytes_and_atomicity(tmp_path: pathlib.Path) -> None:
    source = tmp_path / "installed.json"
    source.write_bytes(b'{"rows":[]}')
    scratch = tmp_path / "monitor"
    copy = claude_worker.monitor.stage_active_copy(scratch, source, "ab" * 16)
    assert copy == scratch / f"active-{'ab' * 16}.json"
    assert copy.read_bytes() == source.read_bytes()
    assert not list(scratch.glob("*.tmp"))
    # Idempotent overwrite (each monitor pass restages).
    source.write_bytes(b'{"rows":[1]}')
    again = claude_worker.monitor.stage_active_copy(scratch, source, "ab" * 16)
    assert again.read_bytes() == b'{"rows":[1]}'


def test_prepare_window_dir_full_root_passthrough(tmp_path: pathlib.Path) -> None:
    replay = tmp_path / "replay"
    tests.craft.write_run(replay, 1_000, [0, 8 * _H_NS])
    spans = claude_worker.monitor.read_run_spans(replay)
    sel = claude_worker.monitor.select_window(spans)
    assert sel is not None and sel.is_full_root
    out = claude_worker.monitor.prepare_window_dir(tmp_path / "monitor", sel, replay)
    assert out == replay, "all runs selected => the harness gets the root"


def test_prepare_window_dir_subset_symlinks_and_rebuild(tmp_path: pathlib.Path) -> None:
    replay = tmp_path / "replay"
    old_epoch = 1_000
    new_epoch = 200 * _H_NS
    tests.craft.write_run(replay, old_epoch, [0, _H_NS])  # far outside the window
    tests.craft.write_run(replay, new_epoch, [0, 8 * _H_NS])
    sel = claude_worker.monitor.select_window(claude_worker.monitor.read_run_spans(replay))
    assert sel is not None and not sel.is_full_root
    scratch = tmp_path / "monitor"
    window = claude_worker.monitor.prepare_window_dir(scratch, sel, replay)
    assert window == scratch / "window"
    names = sorted(p.name for p in window.iterdir())
    assert names == [f"run-{new_epoch}"]
    link = window / f"run-{new_epoch}"
    assert link.is_symlink() and link.resolve() == (replay / f"run-{new_epoch}").resolve()
    # The link resolves to REAL capture: a reader through it sees ticks.
    spans_via_link = claude_worker.monitor.read_run_spans(window)
    assert len(spans_via_link) == 1 and spans_via_link[0].duration_ns == 8 * _H_NS
    # Rebuild drops stale links (window moved on).
    (window / "run-999").symlink_to(replay / f"run-{old_epoch}")
    rebuilt = claude_worker.monitor.prepare_window_dir(scratch, sel, replay)
    assert sorted(p.name for p in rebuilt.iterdir()) == [f"run-{new_epoch}"]


def test_summary_line_carries_verdict_and_numbers() -> None:
    sel = claude_worker.monitor.select_window([_span(0, 8)])
    assert sel is not None
    line = claude_worker.monitor.summary_line("cd" * 16, sel, _harness(-150.0, 20.0), True)
    assert "cd" * 16 in line
    assert "net_pnl_usd=-150.0" in line
    assert "8.0 h coverage" in line
    assert "ROLLBACK TRIGGERED" in line
    holding = claude_worker.monitor.summary_line("cd" * 16, sel, _harness(5.0, 20.0), False)
    assert "verdict=holding" in holding


# ---- registry accessor (state.committed_rulesets) ------------------------


def test_committed_rulesets_order_and_filters(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    a, b, c = "aa" * 32, "bb" * 32, "cc" * 32
    st.stage_ruleset(a, "a.json", "a.report.json", "auto", 100)
    st.mark_ruleset_committed(a, ts=110)
    st.stage_ruleset(b, "b.json", "b.report.json", "session", 200)
    st.mark_ruleset_committed(b, ts=210)
    st.stage_ruleset(c, "c.json", "c.report.json", "auto", 300)  # staged, never committed
    rows = st.committed_rulesets()
    assert [row[0] for row in rows] == [b, a], "most recently committed first"
    assert rows[0] == (b, "b.json", "b.report.json", 200, 210)
    # A supersede restage CLEARS the commit: the row leaves the set.
    st.stage_ruleset(b, "b.json", "b.report.json", "auto", 400)
    assert [row[0] for row in st.committed_rulesets()] == [a]
    # Same-second commit tie: deterministic (staged_ts DESC tiebreak).
    st.mark_ruleset_committed(b, ts=110)
    tied = st.committed_rulesets()
    assert [row[0] for row in tied] == [b, a], "staged_ts 400 > 100 breaks the tie"
    st.close()
