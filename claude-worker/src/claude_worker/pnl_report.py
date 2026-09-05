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

ICDP I6 (2026-09-03) — the nightly lane REVIVED. Two findings killed it
since Aug-23: the launchd context had no ``multivenue-engine`` on PATH
(``FileNotFoundError`` ×6 in restart.log — fixed in daily-restart.sh),
and a whole-root replay OOMs past ~27 GB (CLAUDE.md ops debt c). The
lane now runs in DAY MODE (``--closed-day`` / ``--day YYYY-MM-DD``):
every ``run-<epoch>`` of that UTC day is audited ON ITS OWN (bounded),
with the operator's fee tier from ``~/multivenue/fees.toml``
(``--fees``, D2/D6: data, not source; the harness defaults stay 0 and
the fee ladder prints regardless), and the per-run reports are MERGED
into the day pair: additive fields summed per strategy, drawdown =
the worst single run (not additive), ``runs_detail`` keeps every run's
own row, the summary carries each run's stderr (per-venue stale line,
fee ladder, IoC counters). The no-flag path (one ``--dir`` replay of
``--replay-dir``) is unchanged for tests and manual bounded roots.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import datetime
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
import typing

import claude_worker.backtest
import claude_worker.candles
import claude_worker.window_root

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


# ---- ICDP I6: fee tier file + day mode ------------------------------------

FEES_PATH_DEFAULT: str = "~/multivenue/fees.toml"
FEE_VENUES: tuple[str, ...] = ("pm", "bn", "okx", "deribit", "hl", "bybit")


def load_fee_flags(path: pathlib.Path) -> list[str]:
    """``[fees]`` section of ``fees.toml``: ``<venue> = "<maker>:<taker>"``
    (integer bps) → repeatable ``--fee-bps <venue>:<maker>:<taker>`` argv.
    A malformed line is fatal (a silently dropped tier would print a
    zero-fee number as if it were the operator's)."""
    text = path.read_text(encoding="utf-8")
    flags: list[str] = []
    in_fees = False
    for idx, raw in enumerate(text.splitlines()):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("["):
            in_fees = line == "[fees]"
            continue
        if not in_fees:
            continue
        key, sep, val = line.partition("=")
        key = key.strip()
        val = val.strip().strip('"')
        if not sep or key not in FEE_VENUES:
            raise ValueError(f"{path}:{idx + 1}: unknown fees key {key!r}")
        maker, sep2, taker = val.partition(":")
        if not sep2 or not maker.isdigit() or not taker.isdigit():
            raise ValueError(f"{path}:{idx + 1}: want \"<maker>:<taker>\" integer bps")
        flags.extend(("--fee-bps", f"{key}:{int(maker)}:{int(taker)}"))
    return flags


def select_runs(replay_dir: pathlib.Path, day: str) -> list[pathlib.Path]:
    """Run dirs whose epoch (wall ns in the name) falls on UTC ``day``."""
    out: list[pathlib.Path] = []
    if not replay_dir.is_dir():
        return out
    for child in sorted(replay_dir.iterdir()):
        if not child.is_dir() or not child.name.startswith("run-"):
            continue
        try:
            epoch_ns = int(child.name[4:])
        except ValueError:
            continue
        d = datetime.datetime.fromtimestamp(
            epoch_ns / 1e9, tz=datetime.timezone.utc
        ).strftime("%Y-%m-%d")
        if d == day:
            out.append(child)
    return out


_SUM_KEYS = (
    "orders", "fills", "trades", "canceled_end", "rejected_caps", "unroutable",
    "ioc_fills", "ioc_canceled", "ttl_expired",
)
_SUM_USD_KEYS = ("net_usd", "realized_usd", "fees_usd", "markout_usd")


def _usd(v: object) -> float:
    try:
        return float(v)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return 0.0


def merge_reports(day: str, runs: list[tuple[str, dict]]) -> dict:
    """Fold per-run audit-pnl JSONs into one day report (same top-level
    shape, ``audit_pnl_version`` 1, additive keys). Sums are exact to
    the harness's 1e-6 render; ``max_drawdown_usd`` is the WORST single
    run — a day-level drawdown across runs is not defined when each run
    starts flat."""
    by_sid: dict[int, dict] = {}
    vm: dict[str, dict] = {}
    wall_first = None
    wall_last = None
    paper_fills = 0
    paper_net = 0.0
    for name, obj in runs:
        w = obj.get("window") or {}
        wf, wl = w.get("wall_first_ns"), w.get("wall_last_ns")
        if isinstance(wf, int):
            wall_first = wf if wall_first is None else min(wall_first, wf)
        if isinstance(wl, int):
            wall_last = wl if wall_last is None else max(wall_last, wl)
        paper = obj.get("paper") or {}
        paper_fills += int(paper.get("fills", 0) or 0)
        paper_net += _usd(paper.get("net_usd", "0"))
        for row in obj.get("strategies") or []:
            sid = int(row.get("strategy_id", 255))
            acc = by_sid.setdefault(sid, {
                "strategy_id": sid, "label": row.get("label", "unknown"),
                **{k: 0 for k in _SUM_KEYS}, **{k: 0.0 for k in _SUM_USD_KEYS},
                "max_drawdown_usd": 0.0, "trading_days": 1, "runs": 0,
                "fee_ladder_net_usd": [0.0, 0.0, 0.0],
            })
            acc["runs"] += 1
            for k in _SUM_KEYS:
                acc[k] += int(row.get(k, 0) or 0)
            for k in _SUM_USD_KEYS:
                acc[k] += _usd(row.get(k, "0"))
            acc["max_drawdown_usd"] = max(acc["max_drawdown_usd"], _usd(row.get("max_drawdown_usd", "0")))
            ladder = row.get("fee_ladder_net_usd") or []
            for i in range(min(3, len(ladder))):
                acc["fee_ladder_net_usd"][i] += _usd(ladder[i])
        for row in obj.get("vm_by_ruleset") or []:
            h = str(row.get("hash128", "?"))
            acc = vm.setdefault(h, {"hash128": h, "orders": 0, "trades": 0, "net_usd": 0.0, "max_drawdown_usd": 0.0})
            acc["orders"] += int(row.get("orders", 0) or 0)
            acc["trades"] += int(row.get("trades", 0) or 0)
            acc["net_usd"] += _usd(row.get("net_usd", "0"))
            acc["max_drawdown_usd"] = max(acc["max_drawdown_usd"], _usd(row.get("max_drawdown_usd", "0")))
    strategies = []
    for sid in sorted(by_sid):
        acc = by_sid[sid]
        strategies.append({
            **{k: acc[k] for k in ("strategy_id", "label", "runs")},
            **{k: acc[k] for k in _SUM_KEYS},
            **{k: f"{acc[k]:.6f}" for k in _SUM_USD_KEYS},
            "max_drawdown_usd": f"{acc['max_drawdown_usd']:.6f}",
            "trading_days": 1,
            "fee_ladder_net_usd": [f"{v:.6f}" for v in acc["fee_ladder_net_usd"]],
            "per_day_net_usd": [{"day": 0, "net_usd": f"{acc['net_usd']:.6f}"}],
        })
    return {
        "audit_pnl_version": AUDIT_PNL_VERSION,
        "day": day,
        "runs": len(runs),
        "window": {"wall_first_ns": wall_first, "wall_last_ns": wall_last, "utc_days": 1},
        "paper": {"fills": paper_fills, "net_usd": f"{paper_net:.6f}"},
        "strategies": strategies,
        "vm_by_ruleset": [
            {**v, "net_usd": f"{v['net_usd']:.6f}", "max_drawdown_usd": f"{v['max_drawdown_usd']:.6f}"}
            for v in (vm[h] for h in sorted(vm))
        ],
        "runs_detail": [{"run": name, "report": obj} for name, obj in runs],
    }


WINDOW_ROOT_DEFAULT: str = "~/multivenue/backtest-roots/nightly"
# RG3: the window seed's inputs — the operator's regime artifact and the
# worker's candles.db (plan §4.3); either absent ⇒ no seed is written.
REGIME_PATH_DEFAULT: str = "~/multivenue/regime.toml"


def regime_seed_inputs(
    env: collections.abc.Mapping[str, str] | None = None,
) -> tuple[pathlib.Path, pathlib.Path]:
    """``(regime.toml, candles.db)`` for the window seed step."""
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    db = source.get(claude_worker.candles.CANDLES_DB_ENV, "") or claude_worker.candles.DEFAULT_DB_PATH
    return (
        pathlib.Path(REGIME_PATH_DEFAULT).expanduser(),
        pathlib.Path(db).expanduser(),
    )


def _audit_units(
    run_dir: pathlib.Path,
    window_root: pathlib.Path | None,
    report: typing.Callable[[str], None],
) -> list[tuple[str, pathlib.Path, bool]]:
    """(label, dir to audit, is_temporary) per ≤ 2 h window of the run —
    the capture-window law; a run without ticks (or windowing off)
    audits as-is. Every cut carries its own ``regime-seed.tsv`` when the
    artifact + candles.db exist (RG3)."""
    if window_root is None:
        return [(run_dir.name, run_dir, False)]
    try:
        windows = claude_worker.window_root.windows_of(run_dir)
    except claude_worker.window_root.WindowError as exc:
        report(f"pnl-report: {run_dir.name}: cannot window ({exc}) — auditing whole")
        return [(run_dir.name, run_dir, False)]
    if len(windows) <= 1:
        return [(run_dir.name, run_dir, False)]
    units: list[tuple[str, pathlib.Path, bool]] = []
    seed = regime_seed_inputs()
    for lo, hi in windows:
        try:
            cut = claude_worker.window_root.cut_run(run_dir, window_root, lo, hi, seed=seed)
        except claude_worker.window_root.WindowError as exc:
            report(f"pnl-report: {run_dir.name}: window {lo:.0f}..{hi:.0f} s failed ({exc})")
            continue
        units.append((f"{run_dir.name}@{lo:.0f}s", cut, True))
    return units


def run_day(
    replay_dir: pathlib.Path,
    reports_dir: pathlib.Path,
    day: str,
    report: typing.Callable[[str], None],
    run_fn: RunFn | None = None,
    fee_flags: list[str] | None = None,
    window_root: pathlib.Path | None = None,
) -> int:
    """Day mode: one bounded audit-pnl per ≤ 2 h window of every run of
    ``day`` (``window_root`` = where the cuts are materialised, deleted
    after their audit; None = audit each run whole), merged. Nonzero
    when nothing audited cleanly (a failed unit is listed in the report
    and on stderr; the merge still lands for the others)."""
    fn = _default_run_fn if run_fn is None else run_fn
    flags = list(fee_flags or [])
    runs = select_runs(replay_dir, day)
    if not runs:
        report(f"pnl-report: {day}: no run dir under {replay_dir} — nothing to audit")
        return 1
    ok: list[tuple[str, dict]] = []
    failed: list[str] = []
    summaries: list[str] = []
    units: list[tuple[str, pathlib.Path, bool]] = []
    for run_dir in runs:
        units.extend(_audit_units(run_dir, window_root, report))
    for label, unit_dir, temporary in units:
        argv = [claude_worker.backtest.ENGINE_BINARY, "audit-pnl", "--dir", str(unit_dir)] + flags
        code, out, err = fn(argv)
        if temporary:
            shutil.rmtree(unit_dir, ignore_errors=True)
        run_dir = pathlib.Path(label)
        body = out.strip()
        obj = None
        if code == 0 and body:
            try:
                obj = json.loads(body)
            except ValueError:
                obj = None
        if not isinstance(obj, dict) or obj.get("audit_pnl_version") != AUDIT_PNL_VERSION:
            failed.append(run_dir.name)
            report(f"pnl-report: {run_dir.name}: audit-pnl exited {code} / bad stdout; stderr tail: {err.strip()[-300:]}")
            summaries.append(f"== {run_dir.name}: FAILED (exit {code})\n{err}")
            continue
        ok.append((run_dir.name, obj))
        summaries.append(f"== {run_dir.name}\n{err}")
    reports_dir.mkdir(parents=True, exist_ok=True)
    json_path, summary_path = report_paths(reports_dir, day)
    merged = merge_reports(day, ok)
    merged["failed_runs"] = failed
    merged["fee_flags"] = flags
    json_path.write_text(json.dumps(merged, separators=(",", ":")) + "\n", encoding="utf-8")
    head = [
        f"pnl-report: day {day}: runs audited {len(ok)} failed {len(failed)}",
        f"fee tier flags: {' '.join(flags) if flags else '(none — harness defaults 0/0; read the ladder)'}",
    ]
    for row in merged["strategies"]:
        head.append(
            f"strategy {row['strategy_id']} ({row['label']}): runs={row['runs']} orders={row['orders']} "
            f"fills={row['fills']} trades={row['trades']} net={row['net_usd']} fees={row['fees_usd']} "
            f"worst_run_dd={row['max_drawdown_usd']} ioc_fills={row['ioc_fills']} "
            f"ioc_canceled={row['ioc_canceled']} ttl_expired={row['ttl_expired']} "
            f"ladder(0/1/2 bps)={row['fee_ladder_net_usd']}"
        )
    summary_path.write_text("\n".join(head) + "\n\n" + "\n".join(summaries), encoding="utf-8")
    report(f"pnl-report: {day}: strategies={len(merged['strategies'])} runs={len(ok)} failed={len(failed)} -> {json_path} (+ summary)")
    return 0 if ok else 1


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface; the operator/nightly entrypoint)."""
    parser = argparse.ArgumentParser(prog="claude_worker.pnl_report")
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument("--reports-dir", default=None)
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    parser.add_argument("--closed-day", action="store_true",
                        help="day mode for the UTC day before now (the nightly lane)")
    parser.add_argument("--day", default=None, help="day mode for this UTC day (YYYY-MM-DD)")
    parser.add_argument("--window-root", default=WINDOW_ROOT_DEFAULT,
                        help="day mode: where the ≤ 2 h window cuts are materialised (deleted after each audit)")
    parser.add_argument("--no-windows", action="store_true",
                        help="day mode: audit each run whole (tests / tiny runs only — the 2 h law)")
    parser.add_argument("--fees", default=None,
                        help=f"fees.toml with the operator's tier (default {FEES_PATH_DEFAULT} in day mode when present)")
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

    if args.closed_day or args.day:
        if args.day:
            day = args.day
        else:
            day = (
                datetime.datetime.fromtimestamp(now_ms / 1000, tz=datetime.timezone.utc)
                - datetime.timedelta(days=1)
            ).strftime("%Y-%m-%d")
        fees_path = pathlib.Path(args.fees).expanduser() if args.fees else pathlib.Path(FEES_PATH_DEFAULT).expanduser()
        fee_flags: list[str] = []
        if args.fees or fees_path.is_file():
            fee_flags = load_fee_flags(fees_path)
        window_root = None if args.no_windows else pathlib.Path(args.window_root).expanduser()
        return run_day(replay_dir, reports_dir, day, report, fee_flags=fee_flags, window_root=window_root)
    return run_once(replay_dir, reports_dir, now_ms, report)


if __name__ == "__main__":
    raise SystemExit(main())
