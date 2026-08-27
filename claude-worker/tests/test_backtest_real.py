# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Real-harness integration (design §12, H-D8 centerpiece): drive the
REAL ``multivenue-engine backtest`` release binary through the FROZEN
``claude_worker.backtest.run_backtest`` seam, over the committed golden
fixture at ``tests/fixtures/backtest-real/`` (byte-pinned by the Rust
side: ``committed_python_fixture_matches_the_generator_byte_for_byte``).

This module is ADDITIVE — the 202 baseline tests and their fake-binary
mock are untouched; the G7 §5.1 fake-``multivenue-engine`` shim seam is
retired as of 8h H2 (the fake-binary pattern survives only inside the
frozen 202 as a mock). Auto-skips wherever the release binary is not on
PATH (``docs/local-setup.md`` runbook: put ``target/release`` on PATH),
so pytest stays green everywhere.

Expected numbers are the Rust golden fixture's hand-computed values
(``crates/cli/tests/backtest_harness.rs::build_pnl_capture`` doc):
net +5.0 / trades 2 / days 2 / DD 4.375 / bounds 50.0, 96.8, 96.8.
All are exact binary decimals or parsed identically by ``json.loads``
and the literal below, so ``==`` is safe.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import shutil

import pytest

import claude_worker.backtest

FIXTURE_DIR = pathlib.Path(__file__).parent / "fixtures" / "backtest-real"

pytestmark = pytest.mark.skipif(
    shutil.which(claude_worker.backtest.ENGINE_BINARY) is None,
    reason="multivenue-engine release binary not on PATH (docs/local-setup.md runbook)",
)


def _tmp_ruleset(tmp_path: pathlib.Path) -> pathlib.Path:
    """Copy the committed ruleset into tmp so ``write_report`` lands its
    ``.report.json`` beside a scratch file, never inside the committed
    fixture tree (which stays read-only to the harness)."""
    src = FIXTURE_DIR / "golden-ruleset.json"
    dst = tmp_path / "golden-ruleset.json"
    dst.write_bytes(src.read_bytes())
    return dst


def test_real_binary_frozen_argv_and_schema1_over_committed_fixture(tmp_path):
    outcome = claude_worker.backtest.run_backtest(_tmp_ruleset(tmp_path), FIXTURE_DIR)

    harness = outcome.harness
    assert harness.split == "70/30"
    assert harness.oos_net_pnl_usd == 5.0
    assert harness.oos_trades == 2
    assert harness.oos_trading_days == 2
    assert harness.oos_max_drawdown_usd == 4.375
    assert harness.max_order_notional_usd == 50.0
    assert harness.max_symbol_notional_usd == 96.8
    assert harness.max_total_notional_usd == 96.8

    # The gate matrix over real numbers: profitable, tiny sample —
    # trades gate refuses, everything else passes, verdict False, and
    # the worker report is still written (gate FAIL is a normal outcome).
    assert outcome.gates.pnl_positive is True
    assert outcome.gates.min_trades is False
    assert outcome.gates.min_days is True
    assert outcome.gates.max_drawdown is True
    assert outcome.gates.bounds is True
    assert outcome.all_passed is False
    assert outcome.report_path.is_file()


def test_real_binary_nonzero_exit_maps_to_backtest_error(tmp_path):
    # A bad split is a harness usage error (nonzero exit, empty stdout);
    # the frozen seam must surface it as BacktestError — exactly the
    # contract the retired fake-binary shim used to simulate.
    with pytest.raises(claude_worker.backtest.BacktestError):
        claude_worker.backtest.run_backtest(
            _tmp_ruleset(tmp_path), FIXTURE_DIR, split="70/40"
        )
