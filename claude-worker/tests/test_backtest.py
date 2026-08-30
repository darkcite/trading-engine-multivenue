# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""backtest.py: mocked subprocess seam, canned pass/fail harness reports
(§11), strict contract validation, gate matrix, report file contents.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
import pathlib

import pytest

import claude_worker.backtest

RULESET_BYTES = b'{"name": "t1", "family": "crypto"}\n'


def _ruleset(tmp_path: pathlib.Path) -> tuple[pathlib.Path, str]:
    path = tmp_path / "t1.json"
    path.write_bytes(RULESET_BYTES)
    return path, hashlib.sha256(RULESET_BYTES).hexdigest()


def _harness_stdout(full_hash: str, **overrides: object) -> str:
    obj: dict[str, object] = {
        "schema_version": claude_worker.backtest.REPORT_SCHEMA_VERSION,
        "ruleset_hash": full_hash,
        "split": "70/30",
        "oos": {
            "net_pnl_usd": 12.5,
            "trades": 60,
            "trading_days": 3,
            "max_drawdown_usd": 20.0,
        },
        "bounds": {
            "max_order_notional_usd": 90.0,
            "max_symbol_notional_usd": 200.0,
            "max_total_notional_usd": 800.0,
        },
    }
    obj.update(overrides)
    return json.dumps(obj)


def test_hashes_full_and_128(tmp_path: pathlib.Path) -> None:
    path, full_hash = _ruleset(tmp_path)
    got_full, got_128 = claude_worker.backtest.ruleset_hashes(path)
    assert got_full == full_hash
    assert got_128 == bytes.fromhex(full_hash)[:16]


def test_pass_report_all_gates(tmp_path: pathlib.Path) -> None:
    path, full_hash = _ruleset(tmp_path)
    seen_argv: list[list[str]] = []

    def run_fn(argv: list[str]) -> str:
        seen_argv.append(argv)
        return _harness_stdout(full_hash)

    outcome = claude_worker.backtest.run_backtest(path, tmp_path / "replay", run_fn=run_fn)
    # Seam contract: exact argv (the 8h binary parses precisely this).
    assert seen_argv == [
        [
            claude_worker.backtest.ENGINE_BINARY,
            "backtest",
            "--ruleset",
            str(path),
            "--replay-dir",
            str(tmp_path / "replay"),
            "--split",
            "70/30",
        ]
    ]
    assert outcome.all_passed is True
    assert outcome.gates == claude_worker.backtest.GateResult(True, True, True, True, True)
    assert outcome.report_path == tmp_path / "t1.report.json"

    report = json.loads(outcome.report_path.read_text())
    assert report["schema_version"] == claude_worker.backtest.REPORT_SCHEMA_VERSION
    assert report["ruleset_hash"] == full_hash
    assert report["gates"]["all_passed"] is True
    assert report["oos"]["trades"] == 60
    assert report["thresholds"]["min_trades"] == 50


@pytest.mark.parametrize(
    ("overrides", "failed_gate"),
    [
        (
            {
                "oos": {
                    "net_pnl_usd": -1.0,
                    "trades": 60,
                    "trading_days": 3,
                    "max_drawdown_usd": 20.0,
                }
            },
            "pnl_positive",
        ),
        (
            {
                "oos": {
                    "net_pnl_usd": 0.0,
                    "trades": 60,
                    "trading_days": 3,
                    "max_drawdown_usd": 20.0,
                }
            },
            "pnl_positive",
        ),
        (
            {
                "oos": {
                    "net_pnl_usd": 5.0,
                    "trades": 49,
                    "trading_days": 3,
                    "max_drawdown_usd": 20.0,
                }
            },
            "min_trades",
        ),
        (
            {
                "oos": {
                    "net_pnl_usd": 5.0,
                    "trades": 60,
                    # Operator ruling 2026-08-30: min_trading_days
                    # floor 2 → 1 (MVP tempo; D1-pattern amendment,
                    # comment at GateThresholds) — 0 days still fails.
                    "trading_days": 0,
                    "max_drawdown_usd": 20.0,
                }
            },
            "min_days",
        ),
        (
            {
                "oos": {
                    "net_pnl_usd": 5.0,
                    "trades": 60,
                    "trading_days": 3,
                    # Operator ruling 2026-08-29: $50k research tier (DD 15% = $7,500;
                    # order/sym/total 10k/20k/100k) — frozen pins amended with the
                    # ruling, the D1 pattern.
                    "max_drawdown_usd": 7_501.0,
                }
            },
            "max_drawdown",
        ),
        (
            {
                "bounds": {
                    "max_order_notional_usd": 10_001.0,
                    "max_symbol_notional_usd": 200.0,
                    "max_total_notional_usd": 800.0,
                }
            },
            "bounds",
        ),
        (
            {
                "bounds": {
                    "max_order_notional_usd": 90.0,
                    "max_symbol_notional_usd": 20_001.0,
                    "max_total_notional_usd": 800.0,
                }
            },
            "bounds",
        ),
        (
            {
                "bounds": {
                    "max_order_notional_usd": 90.0,
                    "max_symbol_notional_usd": 200.0,
                    "max_total_notional_usd": 100_001.0,
                }
            },
            "bounds",
        ),
    ],
)
def test_fail_reports_still_written(
    tmp_path: pathlib.Path, overrides: dict[str, object], failed_gate: str
) -> None:
    path, full_hash = _ruleset(tmp_path)
    outcome = claude_worker.backtest.run_backtest(
        path,
        tmp_path / "replay",
        run_fn=lambda _argv: _harness_stdout(full_hash, **overrides),
    )
    assert outcome.all_passed is False
    assert getattr(outcome.gates, failed_gate) is False
    # §6: report written on fail too — and it is not stageable.
    report = json.loads(outcome.report_path.read_text())
    assert report["gates"]["all_passed"] is False
    assert report["gates"][failed_gate] is False


def test_harness_hash_mismatch_rejected(tmp_path: pathlib.Path) -> None:
    path, _full_hash = _ruleset(tmp_path)
    wrong = hashlib.sha256(b"other bytes").hexdigest()
    with pytest.raises(claude_worker.backtest.BacktestError, match="ruleset_hash"):
        claude_worker.backtest.run_backtest(
            path, tmp_path / "r", run_fn=lambda _argv: _harness_stdout(wrong)
        )
    # An untrustworthy report writes NOTHING.
    assert not claude_worker.backtest.report_path_for(path).exists()


def test_harness_schema_version_rejected(tmp_path: pathlib.Path) -> None:
    path, full_hash = _ruleset(tmp_path)
    with pytest.raises(claude_worker.backtest.BacktestError, match="schema_version"):
        claude_worker.backtest.run_backtest(
            path,
            tmp_path / "r",
            run_fn=lambda _argv: _harness_stdout(full_hash, schema_version=2),
        )


def test_harness_garbage_stdout_rejected(tmp_path: pathlib.Path) -> None:
    path, _ = _ruleset(tmp_path)
    with pytest.raises(claude_worker.backtest.BacktestError, match="not JSON"):
        claude_worker.backtest.run_backtest(path, tmp_path / "r", run_fn=lambda _argv: "boom")
    with pytest.raises(claude_worker.backtest.BacktestError, match="JSON object"):
        claude_worker.backtest.run_backtest(path, tmp_path / "r", run_fn=lambda _argv: "[1]")


def test_harness_bool_numbers_rejected(tmp_path: pathlib.Path) -> None:
    path, full_hash = _ruleset(tmp_path)
    bad_oos = {"net_pnl_usd": True, "trades": 60, "trading_days": 3, "max_drawdown_usd": 1.0}
    with pytest.raises(claude_worker.backtest.BacktestError, match="net_pnl_usd"):
        claude_worker.backtest.run_backtest(
            path, tmp_path / "r", run_fn=lambda _argv: _harness_stdout(full_hash, oos=bad_oos)
        )
    bad_trades = {"net_pnl_usd": 5.0, "trades": True, "trading_days": 3, "max_drawdown_usd": 1.0}
    with pytest.raises(claude_worker.backtest.BacktestError, match="trades"):
        claude_worker.backtest.run_backtest(
            path, tmp_path / "r", run_fn=lambda _argv: _harness_stdout(full_hash, oos=bad_trades)
        )


def test_default_run_fn_missing_binary_wrapped(tmp_path: pathlib.Path) -> None:
    with pytest.raises(claude_worker.backtest.BacktestError, match="spawn failed"):
        claude_worker.backtest.default_run_fn(["definitely-not-a-real-binary-8f", "backtest"])


def test_custom_thresholds_apply(tmp_path: pathlib.Path) -> None:
    path, full_hash = _ruleset(tmp_path)
    tight = claude_worker.backtest.GateThresholds(min_trades=100)
    outcome = claude_worker.backtest.run_backtest(
        path,
        tmp_path / "r",
        thresholds=tight,
        run_fn=lambda _argv: _harness_stdout(full_hash),
    )
    assert outcome.gates.min_trades is False
    assert outcome.all_passed is False
