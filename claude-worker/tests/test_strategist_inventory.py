# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Ruling #7(a) digest inventory sections (2026-08-28 remediation plan).

Additive suite: POSITIONS + PER-STRATEGY SHADOW P&L renderers, their
honest-empty forms, and build_digest's backward-compatible section
wiring (None omits the section — the pre-#7a dedupe keys are pinned
unchanged by the existing strategist tests).

Convention: full ``import x`` only.
"""

import json
import pathlib

import claude_worker.strategist


def test_positions_text_honest_when_source_absent() -> None:
    text = claude_worker.strategist.positions_digest_text(None)
    assert text == claude_worker.strategist.POSITIONS_EMPTY_TEXT


def test_positions_text_renders_rows_pairs_and_total() -> None:
    payload: dict[str, object] = {
        "run_dir": "/tmp/run-1",
        "fills_torn": True,
        "positions": [
            {
                "sym": 42,
                "net_qty": 1.5,
                "avg_px": 0.45,
                "mark_px": 0.5,
                "realized_usd": 1.25,
                "unrealized_usd": 0.075,
                "exposure_usd": 0.75,
            }
        ],
        "hip4_pairs": [
            {"yes_sym": 42, "no_sym": 43, "net_qty": 1.5, "exposure_usd": 0.75}
        ],
        "total_exposure_usd": 0.75,
    }
    text = claude_worker.strategist.positions_digest_text(payload)
    assert "sym 42" in text
    assert "pair yes 42 / no 43" in text
    assert "total exposure $0.75" in text
    assert "torn tail" in text


def test_positions_text_flat_payload_is_flat_not_error() -> None:
    text = claude_worker.strategist.positions_digest_text(
        {"positions": [], "hip4_pairs": [], "total_exposure_usd": 0.0}
    )
    assert "flat" in text
    assert "total exposure $0.00" in text


def test_gather_positions_payload_none_without_runs(tmp_path: pathlib.Path) -> None:
    # Failure-mode-of-absence: an empty replay root is None, not a raise.
    assert claude_worker.strategist.gather_positions_payload(tmp_path) is None


def test_pnl_text_honest_when_no_reports(tmp_path: pathlib.Path) -> None:
    assert (
        claude_worker.strategist.pnl_digest_text(tmp_path / "absent")
        == claude_worker.strategist.PNL_EMPTY_TEXT
    )


def test_pnl_text_renders_latest_report(tmp_path: pathlib.Path) -> None:
    (tmp_path / "pnl-2026-08-22.json").write_text("{}")
    (tmp_path / "pnl-2026-08-23.json").write_text(
        json.dumps(
            {
                "audit_pnl_version": 1,
                "runs": 2,
                "paper": {"fills": 3, "net_usd": "-1.5"},
                "strategies": [
                    {"strategy_id": 0, "label": "latency-arb", "fills": 3}
                ],
            }
        )
    )
    text = claude_worker.strategist.pnl_digest_text(tmp_path)
    assert "pnl-2026-08-23.json" in text  # newest by name wins
    assert "runs=2" in text
    assert "latency-arb" in text
    assert "paper:" in text


def test_pnl_text_malformed_report_is_honest(tmp_path: pathlib.Path) -> None:
    (tmp_path / "pnl-2026-08-24.json").write_text("not json")
    text = claude_worker.strategist.pnl_digest_text(tmp_path)
    assert "unreadable" in text
    assert "pnl-2026-08-24.json" in text


def test_build_digest_carries_inventory_sections(tmp_path: pathlib.Path) -> None:
    digest = claude_worker.strategist.build_digest(
        tmp_path,
        None,
        {"btc-daily": 42},
        positions="  sym 42  net 1.000000",
        pnl="  report pnl-2026-08-23.json  runs=2",
    )
    assert "POSITIONS (paper netting, current run):" in digest
    assert "PER-STRATEGY SHADOW P&L (latest nightly report):" in digest
    assert "sym 42  net 1.000000" in digest


def test_build_digest_omits_sections_when_none(tmp_path: pathlib.Path) -> None:
    # Backward compatibility: None keeps the pre-#7a byte stream (the
    # SQLite dedupe key for legacy callers is untouched).
    digest = claude_worker.strategist.build_digest(tmp_path, None, {"btc-daily": 42})
    assert "POSITIONS" not in digest
    assert "SHADOW P&L" not in digest
