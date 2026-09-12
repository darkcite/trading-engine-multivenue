# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_ledger.py — the O5 / G6.1 calibration instrument.

The ledger is an ACCUMULATING measurement across windows, so the two
properties that make it trustworthy are that merging is idempotent (a
re-cut window must not double-count, and must never un-settle a row
that already has its outcome) and that the calibration is computed over
settled rows only. The third is the verdict's direction: INSUFFICIENT
is not a pass, and it never waits.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib

import pytest

import claude_worker.bin15_ledger
import claude_worker.bin15_ref


def _row(ts: int, outcome: int, p: int, y: int, tau: int = 900_000_000_000, fam: int = 0):
    return claude_worker.bin15_ledger.Row(
        ts_ns=ts,
        family=fam,
        outcome=outcome,
        tau_ns=tau,
        p_hat_1e6=p,
        p_raw_1e6=p,
        arm=0,
        y=y,
    )


#: One instance is sampled every 30 s from tau = 900 s down, so it
#: appears in ALL THREE phases. A ledger with rows in only one phase is
#: an artefact of a test, and the gate is right to call it incomplete.
_TAUS: tuple[int, ...] = (900_000_000_000, 300_000_000_000, 60_000_000_000)


def _instance_rows(outcome: int, p: int, y: int) -> list:
    """One instance's three samples, one per phase."""
    return [
        _row(outcome * 10 + i, outcome, p, y, tau=tau) for i, tau in enumerate(_TAUS)
    ]


def _sidecar(tmp_path: pathlib.Path, name: str, rows: list[dict]) -> pathlib.Path:
    p = tmp_path / name
    p.write_text(json.dumps({"detail_version": 7, "bin15_ledger": rows}), encoding="utf-8")
    return p


def test_a_sidecar_without_the_block_contributes_nothing() -> None:
    """Every member other than bin15 writes no block, and so does a
    bin15 run that never priced."""
    assert claude_worker.bin15_ledger.rows_from_sidecar('{"detail_version":7}') == []
    assert claude_worker.bin15_ledger.rows_from_sidecar(
        '{"detail_version":7,"bin15_ledger":[]}'
    ) == []


def test_a_null_payout_reads_as_unsettled_not_as_zero() -> None:
    """`y: null` is "this window cannot derive the payout". Reading it
    as 0 would score every unfinished instance as OTM."""
    rows = claude_worker.bin15_ledger.rows_from_sidecar(
        json.dumps(
            {
                "bin15_ledger": [
                    {
                        "ts_ns": 1,
                        "family": 0,
                        "outcome": 2650,
                        "tau_ns": 900_000_000_000,
                        "p_hat_1e6": 500_000,
                        "p_raw_1e6": 500_000,
                        "arm": 0,
                        "y": None,
                    }
                ]
            }
        )
    )
    assert len(rows) == 1
    assert rows[0].y == claude_worker.bin15_ledger.Y_UNKNOWN
    assert not rows[0].settled


def test_merging_is_idempotent_and_never_unsettles_a_row() -> None:
    settled = _row(100, 2650, 800_000, 1_000_000)
    unsettled = _row(100, 2650, 800_000, claude_worker.bin15_ledger.Y_UNKNOWN)
    merged, added = claude_worker.bin15_ledger.merge([settled], [unsettled])
    assert added == 0, "the same instant on the same instance is one row"
    assert merged[0].settled, "a re-cut window must not un-settle the ledger"
    # A genuinely new instant appends.
    merged, added = claude_worker.bin15_ledger.merge(merged, [_row(130, 2650, 810_000, 1_000_000)])
    assert added == 1
    assert len(merged) == 2
    # And the file stays sorted, oldest first.
    assert [r.ts_ns for r in merged] == [100, 130]


def test_the_round_trip_through_the_file_is_exact(tmp_path: pathlib.Path) -> None:
    rows = [_row(1, 2650, 123_456, 1_000_000), _row(2, 2651, 0, 0, fam=7)]
    path = tmp_path / "ledger.tsv"
    claude_worker.bin15_ledger.write_ledger(path, rows)
    assert claude_worker.bin15_ledger.read_ledger(path) == rows
    assert not list(tmp_path.glob("*.tmp")), "the temp file must be renamed away"
    # A malformed row is refused, never silently skipped.
    path.write_text("1\t2\t3\n", encoding="utf-8")
    with pytest.raises(ValueError):
        claude_worker.bin15_ledger.read_ledger(path)


def test_an_absent_ledger_reads_as_empty(tmp_path: pathlib.Path) -> None:
    assert claude_worker.bin15_ledger.read_ledger(tmp_path / "nope.tsv") == []


def test_calibration_ignores_unsettled_rows() -> None:
    """An unsettled row has no outcome to be calibrated against;
    counting it would make a window that ends early look like a model
    that is wrong."""
    rows = [
        _row(1, 1, 900_000, 1_000_000),
        _row(2, 2, 900_000, claude_worker.bin15_ledger.Y_UNKNOWN),
    ]
    table = claude_worker.bin15_ledger.calibration(rows)
    early = table[claude_worker.bin15_ref.PHASE_EARLY]
    assert early.rows == 1
    assert early.instances == 1


def test_a_perfectly_calibrated_ledger_has_zero_error() -> None:
    # Ten instances at p̂ = 0.90, nine of which settle ITM.
    rows = [
        r
        for i in range(10)
        for r in _instance_rows(i + 1, 900_000, 1_000_000 if i < 9 else 0)
    ]
    table = claude_worker.bin15_ledger.calibration(rows)
    for t in table:
        assert t.rows == 10, t.name
        assert t.instances == 10, t.name
        assert t.weighted_err_1e6 == 0, f"{t.name}: 0.90 believed, 0.90 realised"


def test_a_biased_ledger_shows_the_bias_in_cents() -> None:
    # Believed 0.90, realised 0.50 — a 40 c miss, in every phase.
    rows = [
        r
        for i in range(10)
        for r in _instance_rows(i + 1, 900_000, 1_000_000 if i % 2 == 0 else 0)
    ]
    table = claude_worker.bin15_ledger.calibration(rows)
    for t in table:
        assert t.weighted_err_1e6 == 400_000, t.name


def test_the_phase_a_row_falls_in_is_its_pricing_horizon() -> None:
    rows = [
        _row(1, 1, 500_000, 1_000_000, tau=900_000_000_000),  # early
        _row(2, 2, 500_000, 1_000_000, tau=300_000_000_000),  # mid
        _row(3, 3, 500_000, 1_000_000, tau=60_000_000_000),   # late
    ]
    table = claude_worker.bin15_ledger.calibration(rows)
    assert [t.rows for t in table] == [1, 1, 1]
    assert [t.name for t in table] == ["early", "mid", "late"]


def test_insufficient_is_not_a_pass_and_a_miss_is_a_fail() -> None:
    perfect = [
        r
        for i in range(10)
        for r in _instance_rows(i + 1, 900_000, 1_000_000 if i < 9 else 0)
    ]
    table = claude_worker.bin15_ledger.calibration(perfect)
    # Ten instances against a 200-instance floor.
    v, reasons = claude_worker.bin15_ledger.verdict(table)
    assert v == "INSUFFICIENT", reasons
    # With the floor lowered, a calibrated ledger PASSES...
    v, _ = claude_worker.bin15_ledger.verdict(table, min_instances=10)
    assert v == "PASS"
    # ...and a biased one FAILS, which outranks any INSUFFICIENT phase.
    biased = [
        r
        for i in range(10)
        for r in _instance_rows(i + 1, 900_000, 1_000_000 if i % 2 == 0 else 0)
    ]
    v, reasons = claude_worker.bin15_ledger.verdict(
        claude_worker.bin15_ledger.calibration(biased), min_instances=10
    )
    assert v == "FAIL", reasons


def test_the_cli_merges_sidecars_and_reports(tmp_path: pathlib.Path, capsys) -> None:
    rows = [
        {
            "ts_ns": 1_000 + i * 10 + k,
            "family": 0,
            "outcome": 2650 + i,
            "tau_ns": tau,
            "p_hat_1e6": 900_000,
            "p_raw_1e6": 899_000,
            "arm": i % 2,
            "y": 1_000_000 if i < 9 else 0,
        }
        for i in range(10)
        for k, tau in enumerate(_TAUS)
    ]
    a = _sidecar(tmp_path, "a.json", rows[:15])
    b = _sidecar(tmp_path, "b.json", rows)
    out = tmp_path / "ledger.tsv"
    rc = claude_worker.bin15_ledger.main(
        ["merge", "--sidecar", str(a), "--sidecar", str(b), "--out", str(out)]
    )
    assert rc == 0
    assert len(claude_worker.bin15_ledger.read_ledger(out)) == 30, "b's overlap deduped"
    # Re-merging the same sidecars adds nothing.
    claude_worker.bin15_ledger.main(["merge", "--sidecar", str(b), "--out", str(out)])
    assert len(claude_worker.bin15_ledger.read_ledger(out)) == 30

    assert claude_worker.bin15_ledger.main(["status", "--ledger", str(out)]) == 0
    assert "settled_instances=10" in capsys.readouterr().out

    # The gate exits NONZERO on anything but PASS.
    assert claude_worker.bin15_ledger.main(["calibration", "--ledger", str(out)]) == 1
    assert (
        claude_worker.bin15_ledger.main(
            ["calibration", "--ledger", str(out), "--min-instances", "10"]
        )
        == 0
    )
    assert "G6.1: PASS" in capsys.readouterr().out


def test_a_missing_sidecar_is_refused(tmp_path: pathlib.Path) -> None:
    assert (
        claude_worker.bin15_ledger.main(
            ["merge", "--sidecar", str(tmp_path / "nope.json"), "--out", str(tmp_path / "l.tsv")]
        )
        == 2
    )
