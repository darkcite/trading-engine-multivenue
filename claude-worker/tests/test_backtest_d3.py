# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V5 pins for the D-3 gate amendment (operator ruling 2026-08-29,
recorded in docs/vm2-plan.md §8 V0: min_trades counts LEGS; position
rulesets additionally require round_trips >= 10 — the D1-pattern
frozen-surface amendment, folded into the min_trades verdict so
GateThresholds/GateResult keep their frozen shapes).

Convention: full ``import x`` only.
"""

import json

import claude_worker.backtest


def _report(**overrides):
    base = {
        "schema_version": 1,
        "ruleset_hash": "ab" * 32,
        "split": "70/30",
        "oos": {
            "net_pnl_usd": 12.5,
            "trades": 60,
            "trading_days": 3,
            "max_drawdown_usd": 10.0,
        },
        "bounds": {
            "max_order_notional_usd": 100.0,
            "max_symbol_notional_usd": 200.0,
            "max_total_notional_usd": 300.0,
        },
    }
    oos_extra = overrides.pop("oos_extra", {})
    base["oos"].update(oos_extra)
    base.update(overrides)
    return claude_worker.backtest.parse_harness_report(
        json.dumps(base), "ab" * 32
    )


def test_pre_v5_report_gates_exactly_as_before():
    """Absent additive keys ⇒ legs := trades, no position gating —
    byte-for-byte pre-V5 verdicts (the frozen-202 compatibility
    law)."""
    report = _report()
    assert report.oos_legs == -1
    assert report.position_rows == 0
    gates = claude_worker.backtest.evaluate_gates(
        report, claude_worker.backtest.GateThresholds()
    )
    assert gates.min_trades is True  # trades 60 >= 50
    assert gates.all_passed is True


def test_min_round_trips_constant_is_the_d3_ruling():
    assert claude_worker.backtest.MIN_ROUND_TRIPS == 10


def test_position_ruleset_requires_round_trips():
    """position_rows > 0 with legs >= 50 but round_trips < 10 must
    FAIL min_trades (the folded D-3 floor)."""
    report = _report(
        position_rows=1,
        oos_extra={"round_trips": 9, "legs": 120},
    )
    gates = claude_worker.backtest.evaluate_gates(
        report, claude_worker.backtest.GateThresholds()
    )
    assert gates.min_trades is False
    assert gates.all_passed is False


def test_position_ruleset_passes_at_ten_round_trips():
    report = _report(
        position_rows=1,
        oos_extra={"round_trips": 10, "legs": 120},
    )
    gates = claude_worker.backtest.evaluate_gates(
        report, claude_worker.backtest.GateThresholds()
    )
    assert gates.min_trades is True
    assert gates.all_passed is True


def test_legs_key_drives_min_trades_when_present():
    """legs < 50 fails even when trades >= 50 — LEGS is the D-3
    counting unit once the harness reports it."""
    report = _report(oos_extra={"round_trips": 0, "legs": 12})
    gates = claude_worker.backtest.evaluate_gates(
        report, claude_worker.backtest.GateThresholds()
    )
    assert gates.min_trades is False


def test_refire_ruleset_ignores_round_trips():
    """position_rows == 0 ⇒ the round-trip floor never applies."""
    report = _report(oos_extra={"round_trips": 0, "legs": 60})
    gates = claude_worker.backtest.evaluate_gates(
        report, claude_worker.backtest.GateThresholds()
    )
    assert gates.min_trades is True


def test_worker_report_carries_additive_keys(tmp_path):
    ruleset = tmp_path / "r.json"
    ruleset.write_text("{}")
    report = _report(
        position_rows=2,
        oos_extra={"round_trips": 11, "legs": 44},
    )
    gates = claude_worker.backtest.evaluate_gates(
        report, claude_worker.backtest.GateThresholds()
    )
    path = claude_worker.backtest.write_report(
        ruleset,
        "ab" * 32,
        report,
        gates,
        claude_worker.backtest.GateThresholds(),
    )
    payload = json.loads(path.read_text())
    assert payload["oos"]["round_trips"] == 11
    assert payload["oos"]["legs"] == 44
    assert payload["position_rows"] == 2
