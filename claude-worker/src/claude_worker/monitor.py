# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Walk-forward monitor logic (design §8.3) — the rollback trigger's
pure substrate. serve-only, like the strategist: composed by
``daemon.ResearchCycle``; never a CLI verb.

Division of labor (the §7.6 threading law, carried): everything here is
pure arithmetic or local file work (run-span reading, window selection,
the scratch copy of the active artifact, the symlinked window dir). The
DAEMON owns the state.db events, the disable/restage/commit frames, and
the inline ``run_backtest`` invocation — all on the serve-loop thread.

The §8.3 row definitions implemented here:

- **metric** — the ACTIVE ruleset's ``net_pnl_usd``/``max_drawdown_usd``
  from a real-harness run over the trailing window with the carved
  ``--split 0/100`` all-OOS form ([`MONITOR_SPLIT`]; plain
  ``run_backtest(split="0/100")`` passthrough — zero worker-code change
  to the frozen ``backtest.py``).
- **window** — trailing [`MONITOR_WINDOW_NS`] (24 h) of CAPTURE anchored
  at the capture's own end (not wall now — a dark engine must not starve
  the monitor of the capture that exists), floor [`MONITOR_FLOOR_NS`]
  (6 h): below the floor the monitor SKIPS, it does not guess. Window
  granularity is RUN-granular: the harness consumes whole run dirs, so a
  run straddling the window start is included whole; the floor counts
  only in-window coverage.
- **threshold** — ``net ≤ −$100`` (½ the risk-policy $200/day
  realized-loss kill line — the AI lane rolls back before the
  engine-level kill would) OR ``dd ≥ $200``. Code constants, never
  prompts.

Report-clobber protection (frozen-surface interaction, H5 interpretation):
the frozen ``run_backtest`` ALWAYS writes ``R.report.json`` next to the
ruleset it scores — pointing it at the registry's artifact path would
overwrite the gates-passed 70/30 promotion report that
``check_stage_binding`` later re-verifies on a rollback restage. The
monitor therefore scores a BYTE-COPY of the active artifact in its own
scratch dir ([`stage_active_copy`]); the hash is unchanged (same bytes),
the promotion report stays pristine.

Run durations are O(1) per file: the PMLR reader is index-by-slot, so a
run's span is ``last_slot.ts_ns − first_slot.ts_ns`` — monotonic-clock
deltas are exact within a run (§3.3), and the serve loop never iterates
a capture.

Convention: full ``import x`` only. No ``from x import y``.
"""

import os
import pathlib
import shutil
import typing

import claude_worker.backtest
import claude_worker.features
import claude_worker.pmlr

# ---- §8.3 constants (design table; thresholds mirror docs/risk-policy.md)

MONITOR_WINDOW_NS: int = 86_400_000_000_000  # trailing 24 h of capture (target)
MONITOR_FLOOR_NS: int = 21_600_000_000_000  # 6 h floor — below it: skip, never guess
MONITOR_NET_PNL_TRIGGER_USD: float = -100.0  # trigger when net <= this
MONITOR_DRAWDOWN_TRIGGER_USD: float = 200.0  # trigger when dd >= this
MONITOR_SPLIT: str = "0/100"  # the §3.4 carved all-OOS form

_TICKS_GLOB: str = "*-ticks.pmlr"
_ACTIVE_COPY_PREFIX: str = "active-"
_WINDOW_DIR_NAME: str = "window"


class RunSpan(typing.NamedTuple):
    """One run dir's wall coverage: ``[epoch_ns, epoch_ns + duration_ns]``.

    ``epoch_ns`` is the wall-clock boot stamp from the dir name;
    ``duration_ns`` is the exact monotonic tick span across the run's
    tick files (0 when the run captured no complete tick)."""

    path: pathlib.Path
    epoch_ns: int
    duration_ns: int

    @property
    def end_ns(self) -> int:
        return self.epoch_ns + self.duration_ns


def _file_span(path: pathlib.Path) -> tuple[int, int] | None:
    """(first_ts, last_ts) of one tick file, O(1) via slot indexing;
    None for empty/unreadable files (tail-tolerant doctrine: a torn or
    foreign file is a skip, never a crash)."""
    try:
        with claude_worker.pmlr.Reader(path) as reader:
            if reader.slot_kind != claude_worker.pmlr.SLOT_KIND_TICK or len(reader) == 0:
                return None
            return reader.tick(0).ts_ns, reader.tick(len(reader) - 1).ts_ns
    except (claude_worker.pmlr.PmlrError, OSError, ValueError):
        return None


def read_run_spans(replay_dir: pathlib.Path) -> list[RunSpan]:
    """All ``run-*`` dirs as [`RunSpan`]s, oldest first (the
    ``features.run_dirs`` order). A run's duration is
    ``max(last_ts) − min(first_ts)`` over its tick files — one shared
    monotonic clock per run (§3.2), so the delta is exact."""
    spans: list[RunSpan] = []
    for run_dir in claude_worker.features.run_dirs(replay_dir):
        first: int | None = None
        last: int | None = None
        for path in sorted(run_dir.glob(_TICKS_GLOB)):
            span = _file_span(path)
            if span is None:
                continue
            file_first, file_last = span
            first = file_first if first is None else min(first, file_first)
            last = file_last if last is None else max(last, file_last)
        duration = 0 if first is None or last is None or last < first else last - first
        epoch_ns = int(run_dir.name[len("run-") :])
        spans.append(RunSpan(path=run_dir, epoch_ns=epoch_ns, duration_ns=duration))
    return spans


class WindowSelection(typing.NamedTuple):
    """The trailing-window pick: which runs, how much in-window capture."""

    runs: list[RunSpan]
    coverage_ns: int
    capture_end_ns: int
    window_start_ns: int
    total_runs: int

    @property
    def is_full_root(self) -> bool:
        """True when every run overlaps the window — the harness can be
        handed the replay ROOT directly (no symlink dir needed)."""
        return len(self.runs) == self.total_runs


def select_window(
    spans: list[RunSpan],
    window_ns: int = MONITOR_WINDOW_NS,
) -> WindowSelection | None:
    """Run-granular trailing-window selection, anchored at the capture's
    own end. ``None`` when there are no runs at all. Straddling runs are
    included whole (the harness consumes whole dirs); ``coverage_ns``
    counts only the in-window portion — the number the floor judges.
    Tickless (duration-0) runs are NEVER selected: they contribute no
    coverage, and the harness refuses a run dir with no tick files (the
    H1 §3.1 run-content rule)."""
    if not spans:
        return None
    capture_end = max(span.end_ns for span in spans)
    window_start = capture_end - window_ns
    selected: list[RunSpan] = []
    coverage = 0
    for span in spans:
        if span.duration_ns == 0 or span.end_ns <= window_start:
            continue
        selected.append(span)
        coverage += span.end_ns - max(span.epoch_ns, window_start)
    return WindowSelection(
        runs=selected,
        coverage_ns=coverage,
        capture_end_ns=capture_end,
        window_start_ns=window_start,
        total_runs=len(spans),
    )


def breach(
    harness: claude_worker.backtest.HarnessReport,
) -> tuple[bool, dict[str, object]]:
    """The §8.3 threshold check, both arms inclusive. Returns
    ``(triggered, metrics)`` — metrics is the event-detail payload
    (values + which arm fired)."""
    net_trigger = harness.oos_net_pnl_usd <= MONITOR_NET_PNL_TRIGGER_USD
    drawdown_trigger = harness.oos_max_drawdown_usd >= MONITOR_DRAWDOWN_TRIGGER_USD
    metrics: dict[str, object] = {
        "net_pnl_usd": harness.oos_net_pnl_usd,
        "max_drawdown_usd": harness.oos_max_drawdown_usd,
        "net_trigger": net_trigger,
        "drawdown_trigger": drawdown_trigger,
    }
    return net_trigger or drawdown_trigger, metrics


def monitor_dir(db_path: pathlib.Path) -> pathlib.Path:
    """The monitor's scratch home: the worker dir + ``monitor`` (the
    candidates-dir convention, H4 interpretation 13)."""
    return db_path.parent / "monitor"


def stage_active_copy(
    scratch_dir: pathlib.Path,
    source: pathlib.Path,
    hash128_hex: str,
) -> pathlib.Path:
    """Byte-copy the ACTIVE artifact into monitor scratch so the frozen
    ``run_backtest`` writes ITS report beside the copy — the registry's
    gates-passed promotion report is never overwritten (module-doc
    rationale). Atomic (tmp + ``os.replace``); same bytes ⇒ same hash."""
    scratch_dir.mkdir(parents=True, exist_ok=True)
    target = scratch_dir / f"{_ACTIVE_COPY_PREFIX}{hash128_hex}.json"
    tmp = target.with_name(target.name + ".tmp")
    tmp.write_bytes(source.read_bytes())
    os.replace(tmp, target)
    return target


def prepare_window_dir(
    scratch_dir: pathlib.Path,
    selection: WindowSelection,
    replay_dir: pathlib.Path,
) -> pathlib.Path:
    """The ``--replay-dir`` input for the monitor run. Full-root
    selections pass the replay ROOT straight through; a subset gets a
    fresh scratch dir of SYMLINKS named exactly like the source run dirs
    (the harness's name-epoch cross-check still holds; run disjointness
    is inherited from the root). Rebuilt per call — stale links from a
    previous window can never leak in."""
    if selection.is_full_root:
        return replay_dir
    window_dir = scratch_dir / _WINDOW_DIR_NAME
    if window_dir.exists():
        shutil.rmtree(window_dir)
    window_dir.mkdir(parents=True)
    for span in selection.runs:
        os.symlink(span.path, window_dir / span.path.name, target_is_directory=True)
    return window_dir


def summary_line(
    hash128_hex: str,
    selection: WindowSelection,
    harness: claude_worker.backtest.HarnessReport,
    triggered: bool,
) -> str:
    """The §7.1 digest 'performance' text: the ACTIVE ruleset's latest
    walk-forward numbers, fed to the strategist on the next cycle."""
    hours = selection.coverage_ns / 3_600_000_000_000
    return (
        f"active {hash128_hex}: walk-forward split {MONITOR_SPLIT}"
        f" over {len(selection.runs)} run(s), {hours:.1f} h coverage:"
        f" net_pnl_usd={harness.oos_net_pnl_usd} trades={harness.oos_trades}"
        f" max_drawdown_usd={harness.oos_max_drawdown_usd}"
        f" verdict={'ROLLBACK TRIGGERED' if triggered else 'holding'}"
    )
