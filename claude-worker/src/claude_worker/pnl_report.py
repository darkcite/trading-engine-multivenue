# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""pnl_report — the M4.3 shadow-P&L report writer (mvp-plan §4-M4).

A standalone MODULE (``python -m claude_worker.pnl_report``): spawns
``multivenue-engine audit-pnl --dir <replay>`` by PATH-resolved name —
the SAME pinned §14 spawn contract as ``backtest.py`` (its
``ENGINE_BINARY`` constant is reused; nothing else in the worker knows
a subprocess is involved; the runner is injectable for tests) — then
writes the day's report pair under the worker reports dir:

- ``pnl-<YYYY-MM-DD>.json``    — the audit-pnl stdout JSON, verbatim
  (schema-checked: ``audit_pnl_version`` 1);
- ``pnl-<YYYY-MM-DD>.summary.txt`` — the audit-pnl stderr human
  summary, verbatim.

Idempotent per UTC day: a re-run refreshes the pair (derived cache —
the PMLR capture stays the truth). The companion ``pnl`` verb (cli.py,
the D1-unfrozen additive verb) is a THIN reader of these files.

Cadence (operator ruling D2): NIGHTLY, wired via M3's launchd window
at C6+ — until then this module is invoked manually after the 00:00Z
restart closes the previous day's run dir. Serialized like every
worker invocation: ``pgrep -f claude-worker`` first, avoid the top of
the hour.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import datetime
import json
import os
import pathlib
import subprocess
import sys
import time
import typing

import claude_worker.backtest

REPORTS_DIR_ENV: str = "CLAUDE_WORKER_REPORTS_DIR"
DEFAULT_REPORTS_DIR: str = "~/multivenue/worker/reports"
REPLAY_DIR_ENV: str = "CLAUDE_WORKER_REPLAY_DIR"
DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"

# The stdout schema this writer accepts (audit-pnl contract).
AUDIT_PNL_VERSION: int = 1

# Generous ceiling for a whole-root replay (offline analytics).
RUN_TIMEOUT_S: int = 1800

RunFn = typing.Callable[[list[str]], tuple[int, str, str]]


def resolve_reports_dir(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    """Reports dir from env (shared with the thin ``pnl`` verb)."""
    e = os.environ if env is None else env
    return pathlib.Path(e.get(REPORTS_DIR_ENV, "") or DEFAULT_REPORTS_DIR).expanduser()


def resolve_replay_dir(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    """Replay root from env (the worker-standard key)."""
    e = os.environ if env is None else env
    return pathlib.Path(e.get(REPLAY_DIR_ENV, "") or DEFAULT_REPLAY_DIR).expanduser()


def report_paths(reports_dir: pathlib.Path, day: str) -> tuple[pathlib.Path, pathlib.Path]:
    """The day's (json, summary) pair."""
    return (
        reports_dir / f"pnl-{day}.json",
        reports_dir / f"pnl-{day}.summary.txt",
    )


def latest_report(reports_dir: pathlib.Path) -> pathlib.Path | None:
    """Newest ``pnl-*.json`` by name (dates sort lexicographically)."""
    if not reports_dir.is_dir():
        return None
    candidates = sorted(reports_dir.glob("pnl-*.json"))
    return candidates[-1] if candidates else None


def _default_run_fn(argv: list[str]) -> tuple[int, str, str]:
    proc = subprocess.run(
        argv,
        capture_output=True,
        text=True,
        timeout=RUN_TIMEOUT_S,
        check=False,
    )
    return proc.returncode, proc.stdout, proc.stderr


def run_once(
    replay_dir: pathlib.Path,
    reports_dir: pathlib.Path,
    now_ms: int,
    report: typing.Callable[[str], None],
    run_fn: RunFn | None = None,
) -> int:
    """One report cycle. Nonzero = the nightly must fail LOUDLY (a
    silent skipped report would read as 'flat day')."""
    fn = _default_run_fn if run_fn is None else run_fn
    argv = [
        claude_worker.backtest.ENGINE_BINARY,
        "audit-pnl",
        "--dir",
        str(replay_dir),
    ]
    code, out, err = fn(argv)
    if code != 0:
        report(f"pnl-report: audit-pnl exited {code}; stderr tail: {err.strip()[-500:]}")
        return 1
    body = out.strip()
    if not body:
        report("pnl-report: audit-pnl produced no stdout — refusing to write a report")
        return 1
    try:
        obj = json.loads(body)
    except ValueError as exc:
        report(f"pnl-report: audit-pnl stdout is not JSON ({exc}) — refused")
        return 1
    if not isinstance(obj, dict) or obj.get("audit_pnl_version") != AUDIT_PNL_VERSION:
        report(
            "pnl-report: unexpected audit_pnl_version"
            f" {obj.get('audit_pnl_version') if isinstance(obj, dict) else '?'}"
            f" (want {AUDIT_PNL_VERSION}) — refused"
        )
        return 1
    day = datetime.datetime.fromtimestamp(
        now_ms / 1000, tz=datetime.timezone.utc
    ).strftime("%Y-%m-%d")
    reports_dir.mkdir(parents=True, exist_ok=True)
    json_path, summary_path = report_paths(reports_dir, day)
    json_path.write_text(body + "\n", encoding="utf-8")
    summary_path.write_text(err, encoding="utf-8")
    strategies = obj.get("strategies")
    n_strategies = len(strategies) if isinstance(strategies, list) else 0
    report(
        f"pnl-report: {day}: strategies={n_strategies} runs={obj.get('runs')}"
        f" -> {json_path} (+ summary)"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface; the operator/nightly entrypoint)."""
    parser = argparse.ArgumentParser(prog="claude_worker.pnl_report")
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument("--reports-dir", default=None)
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    args = parser.parse_args(argv)
    replay_dir = (
        pathlib.Path(args.replay_dir).expanduser()
        if args.replay_dir
        else resolve_replay_dir()
    )
    reports_dir = (
        pathlib.Path(args.reports_dir).expanduser()
        if args.reports_dir
        else resolve_reports_dir()
    )
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)

    def report(line: str) -> None:
        print(line, file=sys.stderr)

    return run_once(replay_dir, reports_dir, now_ms, report)


if __name__ == "__main__":
    raise SystemExit(main())
