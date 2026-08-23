"""pnl_report module (M4.3) + the D1-unfrozen thin ``pnl`` verb.

Module: injectable-runner contract (argv shape, schema refusals,
report-pair writing, per-day idempotence, loud failure). Verb: thin
file reader only — latest/dated selection, summary-vs-json, exit 2
when nothing exists. One PATH-gated real-binary end-to-end rides the
same skip law as ``test_backtest_real``.

Convention: full ``import x`` only. No ``from x import y``.
"""

import datetime
import json
import pathlib
import shutil
import struct

import pytest
import typer.testing

import tests.craft

import claude_worker.backtest
import claude_worker.cli
import claude_worker.pmlr
import claude_worker.pnl_report

_RUNNER = typer.testing.CliRunner()

GOOD_JSON = (
    '{"audit_pnl_version":1,"runs":2,"paper":{"fills":0,"net_usd":"0.0"},'
    '"strategies":[{"strategy_id":0,"label":"latency-arb"}],'
    '"vm_by_ruleset":[],"vm_orders_no_hash":0}'
)
NOW_MS = 1_787_500_000_000  # fixed test clock


def day_of(now_ms: int) -> str:
    return datetime.datetime.fromtimestamp(
        now_ms / 1000, tz=datetime.timezone.utc
    ).strftime("%Y-%m-%d")


def ok_run_fn(argv: list[str]) -> tuple[int, str, str]:
    # The §14 spawn contract: PATH-resolved name + the audit-pnl argv.
    assert argv[0] == claude_worker.backtest.ENGINE_BINARY
    assert argv[1] == "audit-pnl" and argv[2] == "--dir"
    return 0, GOOD_JSON + "\n", "audit-pnl: summary line\n"


def run_once(tmp_path: pathlib.Path, run_fn) -> tuple[int, list[str], pathlib.Path]:
    lines: list[str] = []
    reports = tmp_path / "reports"
    rc = claude_worker.pnl_report.run_once(
        tmp_path / "logs", reports, NOW_MS, lines.append, run_fn=run_fn
    )
    return rc, lines, reports


# ---- module ---------------------------------------------------------------


def test_run_once_writes_the_report_pair(tmp_path):
    rc, lines, reports = run_once(tmp_path, ok_run_fn)
    assert rc == 0
    day = day_of(NOW_MS)
    json_path = reports / f"pnl-{day}.json"
    summary_path = reports / f"pnl-{day}.summary.txt"
    assert json.loads(json_path.read_text())["audit_pnl_version"] == 1
    assert summary_path.read_text() == "audit-pnl: summary line\n"
    assert any("strategies=1" in l and "runs=2" in l for l in lines)


def test_run_once_is_idempotent_per_day_and_refreshes(tmp_path):
    rc, _, reports = run_once(tmp_path, ok_run_fn)
    assert rc == 0

    def fresh(argv):
        return 0, GOOD_JSON.replace('"runs":2', '"runs":3') + "\n", "s2\n"

    rc2, _, _ = run_once(tmp_path, fresh)
    assert rc2 == 0
    day = day_of(NOW_MS)
    assert len(list(reports.glob("pnl-*.json"))) == 1, "same-day pair refreshed"
    assert json.loads((reports / f"pnl-{day}.json").read_text())["runs"] == 3
    assert (reports / f"pnl-{day}.summary.txt").read_text() == "s2\n"


def test_run_once_fails_loudly_on_nonzero_exit(tmp_path):
    rc, lines, reports = run_once(tmp_path, lambda argv: (1, "", "boom\n"))
    assert rc == 1
    assert not (reports / f"pnl-{day_of(NOW_MS)}.json").exists()
    assert any("exited 1" in l for l in lines)


def test_run_once_refuses_empty_or_bad_or_wrong_version_stdout(tmp_path):
    for out in ("", "not json", '{"audit_pnl_version":2}'):
        rc, _, reports = run_once(tmp_path, lambda argv, o=out: (0, o, ""))
        assert rc == 1
        assert not (reports / f"pnl-{day_of(NOW_MS)}.json").exists()


# ---- the thin verb --------------------------------------------------------


def seed_reports(tmp_path: pathlib.Path) -> pathlib.Path:
    reports = tmp_path / "reports"
    reports.mkdir(parents=True)
    (reports / "pnl-2026-08-22.json").write_text(GOOD_JSON + "\n")
    (reports / "pnl-2026-08-22.summary.txt").write_text("day22 summary\n")
    (reports / "pnl-2026-08-23.json").write_text(
        GOOD_JSON.replace('"runs":2', '"runs":9') + "\n"
    )
    return reports


def invoke(reports: pathlib.Path, args: list[str]):
    return _RUNNER.invoke(
        claude_worker.cli.app,
        args,
        env={claude_worker.pnl_report.REPORTS_DIR_ENV: str(reports)},
    )


def test_pnl_verb_prints_latest_falling_back_to_json(tmp_path):
    reports = seed_reports(tmp_path)
    # Newest (08-23) has no summary file -> raw JSON fallback.
    res = invoke(reports, ["pnl"])
    assert res.exit_code == 0
    assert '"runs":9' in res.stdout


def test_pnl_verb_dated_summary_and_json_modes(tmp_path):
    reports = seed_reports(tmp_path)
    res = invoke(reports, ["pnl", "--date", "2026-08-22"])
    assert res.exit_code == 0
    assert "day22 summary" in res.stdout
    res = invoke(reports, ["pnl", "--date", "2026-08-22", "--json"])
    assert res.exit_code == 0
    assert '"runs":2' in res.stdout


def test_pnl_verb_missing_report_is_usage_error(tmp_path):
    reports = seed_reports(tmp_path)
    res = invoke(reports, ["pnl", "--date", "2026-01-01"])
    assert res.exit_code == claude_worker.cli.EXIT_USAGE
    empty = tmp_path / "empty"
    empty.mkdir()
    res = invoke(empty, ["pnl"])
    assert res.exit_code == claude_worker.cli.EXIT_USAGE


# ---- real-binary end-to-end (the test_backtest_real skip law) -------------

_ORDER = struct.Struct("<QIBB2xqqQBB")


def write_orders(path: pathlib.Path, epoch_ns: int, rows: list[tuple]) -> None:
    """v2 kind-3 file: rows = (ts, sym, side, px, qty, oid, strategy)."""
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately
        claude_worker.pmlr.MAGIC, 2, claude_worker.pmlr.SLOT_KIND_ORDER, epoch_ns
    )
    blob = bytearray(header + bytes(claude_worker.pmlr.HEADER_SIZE - len(header)))
    for ts, sym, side, px, qty, oid, strategy in rows:
        slot = _ORDER.pack(ts, sym, side, 0, px, qty, oid, 0, strategy)
        blob.extend(slot + bytes(claude_worker.pmlr.SLOT_SIZE - len(slot)))
    path.write_bytes(bytes(blob))


@pytest.mark.skipif(
    shutil.which("multivenue-engine") is None,
    reason="multivenue-engine release binary not on PATH (docs/local-setup.md runbook)",
)
def test_real_binary_end_to_end(tmp_path):
    epoch = 1_700_000_000_000_000_000
    run_dir = tmp_path / "logs" / f"run-{epoch}"
    run_dir.mkdir(parents=True)
    (run_dir / "instrument-manifest.tsv").write_text("42\tPMTOK\n")
    # Anchor + a post-Δ crossing tick (craft px 400k/420k; Δpm 200 ms).
    tests.craft.write_ticks(
        run_dir / "pm-ticks.pmlr", [1_000, 300_000_000], epoch
    )
    write_orders(
        run_dir / "engine-orders.pmlr",
        epoch,
        [(2_000, 42, 0, 500_000, 1_000_000, 7, 0)],
    )
    rc = claude_worker.pnl_report.main(
        [
            "--replay-dir", str(tmp_path / "logs"),
            "--reports-dir", str(tmp_path / "reports"),
            "--now-ms", str(NOW_MS),
        ]
    )
    assert rc == 0
    body = (tmp_path / "reports" / f"pnl-{day_of(NOW_MS)}.json").read_text()
    obj = json.loads(body)
    assert obj["audit_pnl_version"] == 1
    assert obj["strategies"][0]["strategy_id"] == 0
    assert obj["strategies"][0]["fills"] == 1
