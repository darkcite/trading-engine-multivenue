# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""hyparb_ladder (HYPARB H6) + its pnl_report wiring.

The ladder's rungs are REWRITES of the operator's artifact (one grammar,
parsed by the engine); every rung's replay is the frozen spawn shape
with ``--member hyparb``; a failed rung is marked, never summed as a
smaller number; and the day report gains the ``hyparb`` key only when
the ladder is asked for.

Convention: full ``import x`` only. No ``from x import y``.
"""

import datetime
import json
import pathlib

import pytest

import claude_worker.backtest
import claude_worker.hyparb_ladder
import claude_worker.pnl_report

REPO = pathlib.Path(__file__).resolve().parents[2]
EXAMPLE = (REPO / "hyparb.toml.example").read_text(encoding="utf-8")

SCHEMA1 = (
    '{"schema_version":1,"ruleset_hash":"ab","split":"0/100","oos":{"net_pnl_usd":%s,'
    '"trades":%d,"trading_days":1,"max_drawdown_usd":0.0,"round_trips":0,"legs":%d},'
    '"bounds":{},"position_rows":1}'
)
COUNTERS = (
    "member: hyparb pool_events=9 pool_refused=0 maps_loaded=1 maps_refused=0 evaluations=3 "
    "arbs=%d side_buy=%d side_sell=%d skipped_below_min=0 skipped_not_live=0 skipped_no_hedge=0 "
    "skipped_inflight=0 skipped_cooldown=0 skipped_halted=0 size_capped=%d amm_fills=1 hedges=1 "
    "hedges_perp=1 hedges_spot=0 hedge_fills=1 hedges_missed=0 flattens=0 inventory_breaches=0 "
    "amm_judged_fills=1 amm_canceled=0 amm_partial=0 amm_not_live=0 gas_usd=%s gas_oos_usd=%s "
    "pnl_predicted_usd=0.9 funding_earned_usd=0.0 orders_emitted=2 regime=not-replayed(v1)\n"
)


def _rung_of(argv: list[str]) -> str:
    text = pathlib.Path(argv[argv.index("--hyparb") + 1]).read_text(encoding="utf-8")
    if "depth_cap_enabled = 0" in text:
        return "r0"
    if "lag_ns = 1\n" in text:
        return "r1"
    if "gas_p50_usd_1e6 = 0" in text:
        return "r2"
    return "r3"


def ladder_fn(argv: list[str]) -> tuple[int, str, str]:
    assert argv[0] == claude_worker.backtest.ENGINE_BINARY
    assert argv[1:5] == ["backtest", "--member", "hyparb", "--hyparb"]
    assert argv[6] == "--replay-dir" and argv[8:10] == ["--split", "0/100"]
    net = {"r0": "5.0", "r1": "2.0", "r2": "1.0", "r3": "0.5"}[_rung_of(argv)]
    return 0, SCHEMA1 % (net, 2, 2) + "\n", COUNTERS % (1, 1, 0, 1, "0.0098", "0.0098")


def test_rungs_rewrite_only_their_keys_and_keep_every_other_byte():
    rungs = dict(claude_worker.hyparb_ladder.RUNGS)
    r0 = claude_worker.hyparb_ladder.rung_artifact(EXAMPLE, rungs["r0"])
    assert "depth_cap_enabled = 0\n" in r0 and "basis_enabled = 0\n" in r0
    assert "gas_p50_usd_1e6 = 0\n" in r0 and "lag_ns = 1\n" in r0
    assert len(r0.splitlines()) == len(EXAMPLE.splitlines())
    assert "# Latency: a hedge IoC lives this long" in r0, "comments survive"
    assert claude_worker.hyparb_ladder.rung_artifact(EXAMPLE, {}) == EXAMPLE, "r3 = the file"
    # A trailing comment on a rewritten line survives too.
    got = claude_worker.hyparb_ladder.rung_artifact("lag_ns = 5  # why\n", {"lag_ns": "1"})
    assert got == "lag_ns = 1  # why\n"


def test_a_key_the_artifact_lacks_or_repeats_refuses_the_rung():
    with pytest.raises(claude_worker.hyparb_ladder.LadderError):
        claude_worker.hyparb_ladder.rung_artifact("x = 1\n", {"lag_ns": "1"})
    with pytest.raises(claude_worker.hyparb_ladder.LadderError):
        claude_worker.hyparb_ladder.rung_artifact("lag_ns = 1\nlag_ns = 2\n", {"lag_ns": "3"})


def test_run_ladder_replays_every_rung_and_parses_net_and_counters(tmp_path):
    rows = claude_worker.hyparb_ladder.run_ladder(
        tmp_path / "unit", EXAMPLE, tmp_path / "s", ["--fee-bps", "hl:1:4"], ladder_fn
    )
    assert [r["rung"] for r in rows] == ["r0", "r1", "r2", "r3"]
    assert [r["oos_net_usd"] for r in rows] == [5.0, 2.0, 1.0, 0.5]
    assert rows[3]["arbs"] == 1 and rows[3]["side_buy"] == 1 and rows[3]["gas_usd"] == 0.0098


def test_a_failed_rung_is_marked_and_not_summed_as_a_smaller_number(tmp_path):
    def fn(argv):
        if _rung_of(argv) == "r2":
            return 1, "", "boom\n"
        return ladder_fn(argv)

    run = claude_worker.hyparb_ladder.run_ladder
    a = run(tmp_path / "u1", EXAMPLE, tmp_path / "s1", [], fn)
    b = run(tmp_path / "u2", EXAMPLE, tmp_path / "s2", [], ladder_fn)
    assert "error" in a[2] and "boom" in a[2]["error"]
    merged = claude_worker.hyparb_ladder.merge_ladder([a, b])
    by = {r["rung"]: r for r in merged}
    assert by["r0"]["units"] == 2 and by["r0"]["oos_net_usd"] == "10.000000"
    assert by["r2"]["units"] == 1 and by["r2"]["failed_units"] == 1
    lines = claude_worker.hyparb_ladder.summary_lines({"paper": [], "ladder": merged})
    assert lines[0] == "hyparb: live paper: no slot-0 row this day"
    assert any(ln.startswith("hyparb: ladder r2: units=1 failed_units=1") for ln in lines)
    assert any("buy/sell=2/0" in ln for ln in lines if "ladder r0" in ln)


def test_a_backtest_without_the_counters_line_fails_the_rung(tmp_path):
    def fn(argv):
        return 0, SCHEMA1 % ("1.0", 1, 1) + "\n", "no member line\n"

    run = claude_worker.hyparb_ladder.run_ladder
    rows = run(tmp_path / "u", EXAMPLE, tmp_path / "s", [], fn)
    assert all("error" in r for r in rows)


RUN_JSON = (
    '{"audit_pnl_version":2,"runs":1,"window":{"wall_first_ns":%d,"wall_last_ns":%d,"utc_days":1},'
    '"paper":{"fills":2,"net_usd":"0.1"},'
    '"strategies":[{"strategy_id":0,"label":"hyparb","origin":1,"orders":2,"fills":2,"trades":2,'
    '"trading_days":1,"net_usd":"0.4","realized_usd":"0.0","fees_usd":"0.01",'
    '"markout_usd":"0.0","max_drawdown_usd":"0.1","canceled_end":0,"rejected_caps":0,'
    '"unroutable":0,"ioc_fills":1,"ioc_canceled":0,"ttl_expired":0,'
    '"fee_ladder_net_usd":["0.4","0.3","0.2"],"per_day_net_usd":[{"day":0,"net_usd":"0.4"}]}],'
    '"vm_by_ruleset":[],"vm_orders_no_hash":0}'
)


def _day(tmp_path: pathlib.Path) -> tuple[pathlib.Path, str, int]:
    d0 = 20_000 * 86_400 * 10**9
    logs = tmp_path / "logs"
    run = d0 + 3_600 * 10**9
    (logs / f"run-{run}").mkdir(parents=True)
    utc = datetime.timezone.utc
    day = datetime.datetime.fromtimestamp(d0 / 1e9, tz=utc).strftime("%Y-%m-%d")
    return logs, day, run


def test_the_day_report_carries_the_ladder_beside_slot_zeros_paper(tmp_path):
    logs, day, run = _day(tmp_path)
    artifact = tmp_path / "hyparb.toml"
    artifact.write_text(EXAMPLE, encoding="utf-8")

    def fn(argv):
        if argv[1] == "audit-pnl":
            return 0, RUN_JSON % (run, run + 10) + "\n", "audit-pnl: ok\n"
        return ladder_fn(argv)

    lines: list[str] = []
    rc = claude_worker.pnl_report.run_day(
        logs, tmp_path / "reports", day, lines.append, run_fn=fn, hyparb_artifact=artifact
    )
    assert rc == 0, lines
    obj = json.loads((tmp_path / "reports" / f"pnl-{day}.json").read_text())
    h = obj["hyparb"]
    assert h["artifact"] == str(artifact)
    assert [r["strategy_id"] for r in h["paper"]] == [0]
    nets = [r["oos_net_usd"] for r in h["ladder"]]
    assert nets == ["5.000000", "2.000000", "1.000000", "0.500000"]
    summary = (tmp_path / "reports" / f"pnl-{day}.summary.txt").read_text()
    assert "hyparb: live PAPER: fills=2 trades=2 net=0.400000" in summary
    assert "hyparb: ladder r3: units=1 net_after_gas=0.500000" in summary


def test_without_the_ladder_the_report_has_no_hyparb_key(tmp_path):
    logs, day, run = _day(tmp_path)

    def fn(argv):
        assert argv[1] == "audit-pnl", "no replay is spawned without the flag"
        return 0, RUN_JSON % (run, run + 10) + "\n", "audit-pnl: ok\n"

    rc = claude_worker.pnl_report.run_day(logs, tmp_path / "reports", day, [].append, run_fn=fn)
    assert rc == 0
    obj = json.loads((tmp_path / "reports" / f"pnl-{day}.json").read_text())
    assert "hyparb" not in obj


def test_an_unreadable_artifact_skips_the_ladder_loudly(tmp_path):
    logs, day, run = _day(tmp_path)

    def fn(argv):
        return 0, RUN_JSON % (run, run + 10) + "\n", "audit-pnl: ok\n"

    lines: list[str] = []
    rc = claude_worker.pnl_report.run_day(
        logs, tmp_path / "reports", day, lines.append, run_fn=fn,
        hyparb_artifact=tmp_path / "absent.toml",
    )
    assert rc == 0
    assert any("hyparb ladder skipped" in ln for ln in lines)
    obj = json.loads((tmp_path / "reports" / f"pnl-{day}.json").read_text())
    assert "hyparb" not in obj
