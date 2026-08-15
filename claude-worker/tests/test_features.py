"""features.py: run discovery, tick features, marks, REST budget, and the
positions/P&L reconstruction over the Rust-writer golden fills (§2/§5.1;
HIP-4 netting mirror per risk-policy).

Scale reminder: px/qty are 1e6 fixed point, cost/P&L 1e12 ("USD units").

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import shutil

import pytest

import claude_worker.features
import claude_worker.pmlr

FIXTURES = pathlib.Path(__file__).parent / "fixtures" / "pmlr"

PM7 = 7
HL_YES = (4 << 24) + 10_810
HL_NO = (4 << 24) + 10_811


def _make_run(tmp_path: pathlib.Path, name: str) -> pathlib.Path:
    run = tmp_path / "replay" / name
    run.mkdir(parents=True)
    shutil.copy(FIXTURES / "ticks_v2.pmlr", run / "pm-ticks.pmlr")
    shutil.copy(FIXTURES / "fills_v2.pmlr", run / claude_worker.features.ENGINE_FILLS_FILE)
    return run


# ---- run-dir discovery -----------------------------------------------------


def test_run_dir_discovery(tmp_path: pathlib.Path) -> None:
    replay = tmp_path / "replay"
    _make_run(tmp_path, "run-100")
    _make_run(tmp_path, "run-200")
    (replay / "not-a-run").mkdir()
    (replay / "run-junk").mkdir()
    dirs = claude_worker.features.run_dirs(replay)
    assert [d.name for d in dirs] == ["run-100", "run-200"]
    latest = claude_worker.features.latest_run_dir(replay)
    assert latest is not None
    assert latest.name == "run-200"


def test_run_dir_discovery_empty(tmp_path: pathlib.Path) -> None:
    assert claude_worker.features.run_dirs(tmp_path / "nope") == []
    assert claude_worker.features.latest_run_dir(tmp_path / "nope") is None


# ---- features + marks -------------------------------------------------------


def test_collect_run_features_and_marks(tmp_path: pathlib.Path) -> None:
    run = _make_run(tmp_path, "run-100")
    features_dir = tmp_path / "features"
    result = claude_worker.features.collect_run(run, features_dir)

    assert result.torn_files == []
    assert result.marks == {PM7: 510_000, HL_YES: 610_000, HL_NO: 390_000}
    assert [p.name for p in result.feature_paths] == [
        f"{PM7}.json",
        f"{HL_YES}.json",
        f"{HL_NO}.json",
    ]

    obj = json.loads((features_dir / "run-100" / f"{PM7}.json").read_text())
    assert obj["sym"] == PM7
    assert obj["venue"] == 0
    assert obj["ticks"] == 2
    assert obj["first_ts_ns"] == 1_000_000_000
    assert obj["last_ts_ns"] == 2_000_000_000
    assert obj["last_bid_px"] == 500_000
    assert obj["last_ask_px"] == 520_000
    assert obj["last_mid_px"] == 510_000
    assert obj["mean_spread"] == 20_000.0
    assert obj["min_spread"] == 20_000
    assert obj["max_spread"] == 20_000
    assert obj["tick_rate_hz"] == 1.0  # 2 ticks over 1 s

    hl = json.loads((features_dir / "run-100" / f"{HL_YES}.json").read_text())
    assert hl["venue"] == 4
    assert hl["ticks"] == 1
    assert hl["tick_rate_hz"] == 0.0


def test_v1_ticks_pin_venue_zero(tmp_path: pathlib.Path) -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "ticks_v1.pmlr") as r:
        feats = claude_worker.features.tick_features(r)
    # v1 predates the venue byte — Phase-1 capture was Polymarket-only.
    assert feats[PM7].venue == 0


# ---- fills ------------------------------------------------------------------


def test_read_fills(tmp_path: pathlib.Path) -> None:
    run = _make_run(tmp_path, "run-100")
    fills, torn = claude_worker.features.read_fills(run)
    assert torn is False
    assert len(fills) == 5
    assert fills[0].order_id == 101


def test_read_fills_missing_file(tmp_path: pathlib.Path) -> None:
    run = tmp_path / "replay" / "run-100"
    run.mkdir(parents=True)
    fills, torn = claude_worker.features.read_fills(run)
    assert fills == []
    assert torn is False


# ---- REST budget -------------------------------------------------------------


def test_rest_budget_window() -> None:
    now = [0]
    budget = claude_worker.features.RestBudget(2, 1_000, clock_ns=lambda: now[0])
    assert budget.try_acquire() is True
    assert budget.try_acquire() is True
    assert budget.try_acquire() is False
    assert budget.skipped_total == 1
    now[0] = 1_000  # window rolls
    assert budget.try_acquire() is True


def test_rest_budget_validation() -> None:
    with pytest.raises(ValueError):
        claude_worker.features.RestBudget(-1, 1_000)
    with pytest.raises(ValueError):
        claude_worker.features.RestBudget(1, 0)


def test_fetch_secondary_budgeted() -> None:
    budget = claude_worker.features.RestBudget(2, 10**9, clock_ns=lambda: 0)
    calls: list[str] = []

    def get_fn(url: str) -> str | None:
        calls.append(url)
        return None if url == "u2" else f"payload:{url}"

    fetched, skipped = claude_worker.features.fetch_secondary(
        budget, get_fn, ["u1", "u2", "u3", "u4"]
    )
    # Budget of 2: u1 fetched, u2 consumed-but-failed (omitted), u3/u4 skipped.
    assert calls == ["u1", "u2"]
    assert fetched == [("u1", "payload:u1")]
    assert skipped == 2
    assert budget.skipped_total == 2


# ---- positions / P&L ----------------------------------------------------------


def test_positions_from_golden_fills(tmp_path: pathlib.Path) -> None:
    run = _make_run(tmp_path, "run-100")
    fills, _torn = claude_worker.features.read_fills(run)
    positions = claude_worker.features.reconstruct_positions(fills)

    pm = positions[PM7]
    # buy 20 @ 0.48, buy 10 @ 0.50, sell 15 @ 0.52:
    # basis removed 14.6e12 * 15/30 = 7.3e12; realized 7.8e12-7.3e12.
    assert pm.net_qty == 15_000_000
    assert pm.open_cost == 7_300_000_000_000
    assert pm.realized == 500_000_000_000  # $0.50
    assert pm.fills == 3

    assert positions[HL_YES].net_qty == 8_000_000
    assert positions[HL_YES].open_cost == 4_800_000_000_000
    assert positions[HL_NO].net_qty == 5_000_000
    assert positions[HL_NO].realized == 0


def test_position_views_marked() -> None:
    fills = [
        claude_worker.pmlr.FillRec(1, PM7, 0, 480_000, 20_000_000, 1),
        claude_worker.pmlr.FillRec(2, PM7, 0, 500_000, 10_000_000, 2),
        claude_worker.pmlr.FillRec(3, PM7, 1, 520_000, 15_000_000, 3),
    ]
    positions = claude_worker.features.reconstruct_positions(fills)
    views = claude_worker.features.position_views(positions, {PM7: 510_000})
    view = views[PM7]
    assert view.net_qty == 15_000_000
    assert view.mark_px == 510_000
    assert view.realized == 500_000_000_000
    assert view.unrealized == 350_000_000_000  # 15 * (0.51 - 0.486667) exact vs basis
    assert view.exposure == 7_650_000_000_000  # 15 * 0.51
    assert view.avg_px == pytest.approx(486_666.6667, rel=1e-9)
    assert claude_worker.features.to_usd(view.realized) == 0.5


def test_position_view_no_mark_carried_at_cost() -> None:
    fills = [claude_worker.pmlr.FillRec(1, PM7, 0, 486_667, 3_000_000, 1)]
    positions = claude_worker.features.reconstruct_positions(fills)
    views = claude_worker.features.position_views(positions, {})
    view = views[PM7]
    assert view.unrealized == 0
    assert view.exposure == abs(positions[PM7].open_cost)


def test_position_short_and_cover() -> None:
    pos = claude_worker.features.Position(sym=1)
    pos.apply(claude_worker.features.SIDE_ASK, 600_000, 10_000_000)  # short 10 @ 0.60
    assert pos.net_qty == -10_000_000
    assert pos.open_cost == -6_000_000_000_000
    pos.apply(claude_worker.features.SIDE_BID, 500_000, 10_000_000)  # cover @ 0.50
    assert pos.net_qty == 0
    assert pos.open_cost == 0
    assert pos.realized == 1_000_000_000_000  # $1.00


def test_position_flip_long_to_short() -> None:
    pos = claude_worker.features.Position(sym=1)
    pos.apply(claude_worker.features.SIDE_BID, 400_000, 10_000_000)  # long 10 @ 0.40
    pos.apply(claude_worker.features.SIDE_ASK, 450_000, 25_000_000)  # sell 25 @ 0.45
    assert pos.net_qty == -15_000_000  # flipped short 15
    assert pos.open_cost == -6_750_000_000_000  # short basis 15 @ 0.45
    assert pos.realized == 500_000_000_000  # closed 10: (0.45-0.40)*10


def test_position_rounding_conserves_value() -> None:
    pos = claude_worker.features.Position(sym=1)
    pos.apply(claude_worker.features.SIDE_BID, 100_001, 2_000_000)
    pos.apply(claude_worker.features.SIDE_BID, 100_000, 1_000_000)
    pos.apply(claude_worker.features.SIDE_ASK, 100_002, 1_000_000)  # non-exact pro-rata
    pos.apply(claude_worker.features.SIDE_ASK, 100_002, 2_000_000)  # full close
    assert pos.net_qty == 0
    assert pos.open_cost == 0  # remainder retention: final close removes all basis
    # Total: proceeds 3 @ 0.100002 minus cost (2 @ 0.100001 + 1 @ 0.100000)
    # = 300_006_000_000 - 300_002_000_000 exactly, rounding residue included.
    assert pos.realized == 4_000_000


def test_position_rejects_nonpositive_qty() -> None:
    pos = claude_worker.features.Position(sym=1)
    with pytest.raises(ValueError):
        pos.apply(claude_worker.features.SIDE_BID, 100, 0)


# ---- HIP-4 netting -------------------------------------------------------------


def test_hip4_pair_netting(tmp_path: pathlib.Path) -> None:
    run = _make_run(tmp_path, "run-100")
    fills, _ = claude_worker.features.read_fills(run)
    positions = claude_worker.features.reconstruct_positions(fills)
    marks = {PM7: 510_000, HL_YES: 610_000, HL_NO: 390_000}
    views = claude_worker.features.position_views(positions, marks)

    pair_views = claude_worker.features.hip4_pair_views(views, [(HL_YES, HL_NO)])
    assert len(pair_views) == 1
    pv = pair_views[0]
    assert pv.net_qty == 3_000_000  # |yes 8 - no 5|
    assert pv.flattened_qty == 5_000_000  # riskless collateral
    assert pv.exposure == 1_830_000_000_000  # 3 * 0.61 (net leg at yes mark)

    total = claude_worker.features.total_exposure(views, pair_views)
    # PM7 gross 7.65 + paired 1.83 (vs 6.83 gross for the pair unnetted).
    assert total == 7_650_000_000_000 + 1_830_000_000_000


def test_hip4_pair_without_positions_omitted() -> None:
    views: dict[int, claude_worker.features.PositionView] = {}
    assert claude_worker.features.hip4_pair_views(views, [(HL_YES, HL_NO)]) == []
