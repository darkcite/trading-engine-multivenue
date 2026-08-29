# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6 (§1.6, D-8): the OFFLINE COVERAGE AUDIT — does the research
side actually HOLD the data the grammar can express, per universe
class? Read-only over candles.db + the newest run's manifest; prints
one table + hollow-descriptor callouts. The fix for a hollow lane is a
fetcher change — this module only finds and names the holes.

Expectation law (per descriptor, from the channel map's caps):

* candles   expected when CAP_PRICE and NOT CAP_OPT (options have no
  candle fetch lane — their offline price/IV home is iv_digest);
* funding   expected when CAP_FUNDING;
* iv        expected when CAP_OPT (iv_digest);
* depth     expected when CAP_DEPTH (depth_digest).

Presence = at least one row with ``open_ts``/``ts_ms`` inside the
audit window (default 48 h — coverage means LIVE coverage). Lookups
join by DESCRIPTOR alone: §9.4 descriptors are globally unique (venue
prefixes; bare PM token ids), so the venue column is redundant here.

Classes are descriptor prefixes, with options split out (``okx/opt``,
``deribit/opt``, ``binance-opt``): bStocks and TradFi perps surface
under their carrying venue prefixes by design.

Module surface only — never a worker verb::

    python -m claude_worker.coverage_audit
    python -m claude_worker.coverage_audit --window-h 24

Convention: full ``import x`` only.
"""

import argparse
import os
import pathlib
import sqlite3
import sys
import time
import typing

import claude_worker.channel_map
import claude_worker.features
import claude_worker.iv_digest

MS_1H: int = 3_600_000
WINDOW_H_DEFAULT: int = 48
DEFAULT_DB_PATH: str = "~/multivenue/worker/candles.db"
DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"
#: Hollow-descriptor callout cap per class×kind (report brevity).
MAX_CALLOUTS: int = 5

_KINDS: tuple[str, ...] = ("candles", "funding", "iv", "depth")


class ClassCoverage(typing.NamedTuple):
    """One class row: per kind, (present, expected) descriptor
    counts + the first hollow descriptors."""

    total: int
    present: dict[str, int]
    expected: dict[str, int]
    hollow: dict[str, list[str]]


def class_of(desc: str) -> str:
    """Descriptor → audit class (prefix; options split out)."""
    if ":" not in desc:
        return "polymarket"
    prefix, name = desc.split(":", 1)
    if prefix in ("okx", "deribit") and (name.endswith("-C") or name.endswith("-P")):
        return f"{prefix}/opt"
    return prefix


def expectations_of(desc: str) -> dict[str, bool]:
    """The expectation law (module docs)."""
    caps = claude_worker.channel_map.caps_of_descriptor(desc)
    is_opt = bool(caps & claude_worker.channel_map.CAP_OPT)
    return {
        "candles": bool(caps & claude_worker.channel_map.CAP_PRICE) and not is_opt,
        "funding": bool(caps & claude_worker.channel_map.CAP_FUNDING),
        "iv": is_opt,
        "depth": bool(caps & claude_worker.channel_map.CAP_DEPTH),
    }


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    return (
        conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (name,)
        ).fetchone()
        is not None
    )


def _present(
    conn: sqlite3.Connection,
    have_tables: dict[str, bool],
    kind: str,
    desc: str,
    lo_ms: int,
) -> bool:
    if kind == "candles":
        if not have_tables["candles"]:
            return False
        row = conn.execute(
            "SELECT 1 FROM candles WHERE descriptor=? AND open_ts>=? LIMIT 1",
            (desc, lo_ms),
        ).fetchone()
    elif kind == "funding":
        if not have_tables["funding"]:
            return False
        row = conn.execute(
            "SELECT 1 FROM funding WHERE descriptor=? AND ts_ms>=? LIMIT 1",
            (desc, lo_ms),
        ).fetchone()
    elif kind == "iv":
        if not have_tables["iv_digest"]:
            return False
        row = conn.execute(
            "SELECT 1 FROM iv_digest WHERE descriptor=? AND open_ts>=? LIMIT 1",
            (desc, lo_ms),
        ).fetchone()
    else:
        if not have_tables["depth_digest"]:
            return False
        row = conn.execute(
            "SELECT 1 FROM depth_digest WHERE descriptor=? AND open_ts>=? LIMIT 1",
            (desc, lo_ms),
        ).fetchone()
    return row is not None


def audit(
    conn: sqlite3.Connection,
    manifest: dict[tuple[int, int], str],
    lo_ms: int,
) -> dict[str, ClassCoverage]:
    """Manifest × DB → per-class coverage."""
    have_tables = {
        name: _table_exists(conn, name)
        for name in ("candles", "funding", "iv_digest", "depth_digest")
    }
    out: dict[str, ClassCoverage] = {}
    for (_ns, _sym), desc in sorted(manifest.items(), key=lambda item: item[1]):
        cls = class_of(desc)
        cov = out.get(cls)
        if cov is None:
            cov = ClassCoverage(
                0,
                {k: 0 for k in _KINDS},
                {k: 0 for k in _KINDS},
                {k: [] for k in _KINDS},
            )
        expected = expectations_of(desc)
        present: dict[str, int] = dict(cov.present)
        expected_counts: dict[str, int] = dict(cov.expected)
        hollow: dict[str, list[str]] = {k: list(v) for k, v in cov.hollow.items()}
        for kind in _KINDS:
            if not expected[kind]:
                continue
            expected_counts[kind] += 1
            if _present(conn, have_tables, kind, desc, lo_ms):
                present[kind] += 1
            elif len(hollow[kind]) < MAX_CALLOUTS:
                hollow[kind].append(desc)
        out[cls] = ClassCoverage(cov.total + 1, present, expected_counts, hollow)
    return out


def render(coverage: dict[str, ClassCoverage]) -> list[str]:
    """Coverage → report lines (one class per line + callouts)."""
    lines: list[str] = []
    header = f"{'class':<16} {'n':>4}"
    for kind in _KINDS:
        header += f" {kind:>14}"
    lines.append(header)
    hollow_total = 0
    for cls in sorted(coverage):
        cov = coverage[cls]
        line = f"{cls:<16} {cov.total:>4}"
        for kind in _KINDS:
            exp = cov.expected[kind]
            if exp == 0:
                line += f" {'-':>14}"
            else:
                cell = f"{cov.present[kind]}/{exp}"
                line += f" {cell:>14}"
        lines.append(line)
    for cls in sorted(coverage):
        cov = coverage[cls]
        for kind in _KINDS:
            missing = cov.expected[kind] - cov.present[kind]
            if missing <= 0:
                continue
            hollow_total += missing
            shown = ", ".join(cov.hollow[kind])
            more = missing - len(cov.hollow[kind])
            suffix = f" (+{more} more)" if more > 0 else ""
            lines.append(f"HOLLOW {cls}/{kind}: {missing} missing — {shown}{suffix}")
    lines.append(f"hollow-lanes-total={hollow_total}")
    return lines


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.coverage_audit")
    parser.add_argument("--db", default=None)
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument("--window-h", type=int, default=WINDOW_H_DEFAULT)
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    args = parser.parse_args(argv)
    env = os.environ
    db_path = pathlib.Path(
        args.db or env.get("CLAUDE_WORKER_CANDLES_DB", "") or DEFAULT_DB_PATH
    ).expanduser()
    replay_root = pathlib.Path(
        args.replay_dir or env.get("CLAUDE_WORKER_REPLAY_DIR", "") or DEFAULT_REPLAY_DIR
    ).expanduser()
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
    lo_ms = now_ms - args.window_h * MS_1H

    run_dir = claude_worker.features.latest_run_dir(replay_root)
    if run_dir is None:
        print(f"coverage: no run dirs under {replay_root}", file=sys.stderr)
        return 1
    manifest = claude_worker.iv_digest.read_manifest(run_dir)
    if manifest is None:
        print(f"coverage: {run_dir.name}: no instrument manifest", file=sys.stderr)
        return 1
    if not db_path.is_file():
        print(f"coverage: no candles db {db_path}", file=sys.stderr)
        return 1
    conn = sqlite3.connect(db_path)
    try:
        coverage = audit(conn, manifest[0], lo_ms)
    finally:
        conn.close()
    for line in render(coverage):
        print(line)
    print(
        f"coverage: run={run_dir.name} window-h={args.window_h} db={db_path}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
