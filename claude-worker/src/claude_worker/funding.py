# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""funding — the WS11 funding-rate HISTORY backfill lane.

A standalone MODULE (``python -m claude_worker.funding``) — NOT a
verb (the frozen CLI surface; the candles/iv_digest/refdata
precedent). One cycle = read the universe file → per funding-bearing
instrument: fetch the venue's NEWEST funding-history page under a
per-venue ``RestBudget`` → ``INSERT OR IGNORE`` into the ``funding``
table BESIDE candles inside ``candles.db`` (§9 keying:
``venue + descriptor``; historical funding points are immutable, so
ignore-on-conflict makes every cycle idempotent).

Funding-bearing selection (the class laws from the engine side):

- ``[binance] usdm``            → ``binance-usdm:<sym>`` — dated
  (``usdm_dated``) futures carry NO funding and are excluded.
- ``[okx] instruments``         → SWAP instIds only (``*-SWAP``).
- ``[deribit] instruments``     → perps only (name contains
  ``PERPETUAL``; spot/dated carry no funding — WS3/WS6 laws).
- ``[hyperliquid] coins``       → perp coins only (no ``@``/``#``).
- ``[bybit] linear``            → ``bybit-linear:<sym>`` (WS9).

Depth per cycle = ONE newest page per instrument (venue page sizes:
BN 1000 pts ≈ 333 d · OKX 100 ≈ 33 d · Deribit range query 30 d ·
HL since ``now − 33 d`` · Bybit 200 ≈ 66 d) — an immediately useful
1–11 month history that stays current under any repeat cadence.
Deeper pagination is a recorded extension, not v1.

Best-effort discipline throughout: transport failure / unusable
body = counted + skipped, never a crash; injectable
``claude_worker.candles.Http`` — no live calls in tests.
"""

import argparse
import collections.abc
import datetime
import json
import os
import pathlib
import sqlite3
import sys
import time
import typing

import httpx

import claude_worker.candles
import claude_worker.features
import claude_worker.fetchers

BUDGET_ENV: str = "CLAUDE_WORKER_FUNDING_BUDGET_PER_H"
BUDGET_PER_H_DEFAULT: int = 30

MS_1D: int = 86_400_000
DERIBIT_RANGE_D: int = 30
HL_RANGE_D: int = 33

_SCHEMA: str = """
CREATE TABLE IF NOT EXISTS funding (
  venue      INTEGER NOT NULL,
  descriptor TEXT    NOT NULL,
  ts_ms      INTEGER NOT NULL,
  rate       REAL    NOT NULL,
  fetched_ts INTEGER NOT NULL,
  PRIMARY KEY (venue, descriptor, ts_ms)
) WITHOUT ROWID;
"""


class FundingTarget(typing.NamedTuple):
    """One funding-bearing instrument in one lane."""

    venue: int
    descriptor: str
    instrument: str


class FundingLane(typing.NamedTuple):
    """One venue funding lane."""

    name: str
    venue: int
    targets: list[FundingTarget]


def ensure_schema(conn: sqlite3.Connection) -> None:
    """Create the ``funding`` table beside candles."""
    conn.executescript(_SCHEMA)


def read_funding_lanes(universe_path: pathlib.Path) -> list[FundingLane] | None:
    """Universe file → funding lanes per the module-docs selection
    laws. ``None`` on unusable file (best-effort law)."""
    lanes_all = claude_worker.candles.read_universe_lanes(universe_path)
    if lanes_all is None:
        return None
    # candles lanes merge classes (usdm+dated share one lane; okx
    # carries spot+swap; …) — re-derive the funding-bearing subset by
    # the name-class laws.
    out: list[FundingLane] = []
    for lane in lanes_all:
        if lane.name == "binance-usdm":
            targets = [
                FundingTarget(t.venue, t.descriptor, t.instrument)
                for t in lane.targets
                if "_" not in t.instrument  # dated names carry `_`
            ]
        elif lane.name == "okx":
            targets = [
                FundingTarget(t.venue, t.descriptor, t.instrument)
                for t in lane.targets
                if t.instrument.endswith("-SWAP")
            ]
        elif lane.name == "deribit":
            targets = [
                FundingTarget(t.venue, t.descriptor, t.instrument)
                for t in lane.targets
                if "PERPETUAL" in t.instrument
            ]
        elif lane.name == "hyperliquid":
            targets = [
                FundingTarget(t.venue, t.descriptor, t.instrument)
                for t in lane.targets
                if not t.instrument.startswith("@") and not t.instrument.startswith("#")
            ]
        elif lane.name == "bybit-linear":
            targets = [
                FundingTarget(t.venue, t.descriptor, t.instrument) for t in lane.targets
            ]
        else:
            continue  # spot lanes carry no funding
        if targets:
            out.append(FundingLane(lane.name, lane.venue, targets))
    return out


# ---- body parsers (skip-never-crash) -------------------------------------


def _rows_of(raw: str) -> list[object] | None:
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if isinstance(obj, list):
        return typing.cast(list[object], obj)
    return None


def parse_bn_funding(raw: str) -> list[tuple[int, float]] | None:
    """``/fapi/v1/fundingRate`` → ``[(fundingTime ms, rate)]``."""
    rows = _rows_of(raw)
    if rows is None:
        return None
    out: list[tuple[int, float]] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        d = typing.cast(dict[str, object], row)
        ts = d.get("fundingTime")
        rate = d.get("fundingRate")
        if isinstance(ts, bool) or not isinstance(ts, int) or not isinstance(rate, str):
            continue
        try:
            out.append((ts, float(rate)))
        except ValueError:
            continue
    return out


def parse_okx_funding(raw: str) -> list[tuple[int, float]] | None:
    """OKX ``funding-rate-history`` → ``[(fundingTime ms, rate)]``
    (quoted ms + quoted rate; ``code`` gated)."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict) or typing.cast(dict[str, object], obj).get("code") != "0":
        return None
    data = typing.cast(dict[str, object], obj).get("data")
    if not isinstance(data, list):
        return None
    out: list[tuple[int, float]] = []
    for row in typing.cast(list[object], data):
        if not isinstance(row, dict):
            continue
        d = typing.cast(dict[str, object], row)
        ts = d.get("fundingTime")
        rate = d.get("fundingRate")
        if not isinstance(ts, str) or not isinstance(rate, str):
            continue
        try:
            out.append((int(ts), float(rate)))
        except ValueError:
            continue
    return out


def parse_deribit_funding(raw: str) -> list[tuple[int, float]] | None:
    """``get_funding_rate_history`` → ``[(timestamp ms,
    interest_8h)]``."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict):
        return None
    result = typing.cast(dict[str, object], obj).get("result")
    if not isinstance(result, list):
        return None
    out: list[tuple[int, float]] = []
    for row in typing.cast(list[object], result):
        if not isinstance(row, dict):
            continue
        d = typing.cast(dict[str, object], row)
        ts = d.get("timestamp")
        rate = d.get("interest_8h")
        if isinstance(ts, bool) or not isinstance(ts, int):
            continue
        if isinstance(rate, bool) or not isinstance(rate, (int, float)):
            continue
        out.append((ts, float(rate)))
    return out


def parse_hl_funding(raw: str) -> list[tuple[int, float]] | None:
    """``fundingHistory`` → ``[(time ms, rate)]`` (quoted rates)."""
    rows = _rows_of(raw)
    if rows is None:
        return None
    out: list[tuple[int, float]] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        d = typing.cast(dict[str, object], row)
        ts = d.get("time")
        rate = d.get("fundingRate")
        if isinstance(ts, bool) or not isinstance(ts, int) or not isinstance(rate, str):
            continue
        try:
            out.append((ts, float(rate)))
        except ValueError:
            continue
    return out


def parse_bybit_funding(raw: str) -> list[tuple[int, float]] | None:
    """``/v5/market/funding/history`` → ``[(ts ms, rate)]``
    (``retCode`` gated; both fields quoted)."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict) or typing.cast(dict[str, object], obj).get("retCode") != 0:
        return None
    result = typing.cast(dict[str, object], obj).get("result")
    if not isinstance(result, dict):
        return None
    rows = typing.cast(dict[str, object], result).get("list")
    if not isinstance(rows, list):
        return None
    out: list[tuple[int, float]] = []
    for row in typing.cast(list[object], rows):
        if not isinstance(row, dict):
            continue
        d = typing.cast(dict[str, object], row)
        ts = d.get("fundingRateTimestamp")
        rate = d.get("fundingRate")
        if not isinstance(ts, str) or not isinstance(rate, str):
            continue
        try:
            out.append((int(ts), float(rate)))
        except ValueError:
            continue
    return out


# ---- fetch + store -------------------------------------------------------


def upsert_points(
    conn: sqlite3.Connection,
    venue: int,
    descriptor: str,
    points: list[tuple[int, float]],
    now_ms: int,
) -> int:
    """``INSERT OR IGNORE`` the points; returns rows actually added
    (historical funding is immutable — conflicts are prior cycles)."""
    added = 0
    for ts_ms, rate in points:
        cur = conn.execute(
            "INSERT OR IGNORE INTO funding (venue,descriptor,ts_ms,rate,fetched_ts)"
            " VALUES (?,?,?,?,?)",
            (venue, descriptor, ts_ms, rate, now_ms),
        )
        added += cur.rowcount if cur.rowcount > 0 else 0
    return added


def _fetch_points(
    http: claude_worker.candles.Http,
    lane: FundingLane,
    target: FundingTarget,
    now_ms: int,
    resume_ms: int | None = None,
) -> list[tuple[int, float]] | None:
    """One newest-page fetch for one instrument. ``None`` = failure.

    ``resume_ms`` (newest stored point) matters only to Hyperliquid:
    its ``fundingHistory`` returns points ASCENDING from ``startTime``
    with a ~500-row page, so a fixed ``now − 33 d`` start returns the
    OLDEST page every cycle and the series stalls ~12 days behind
    (M5-onboarding find, 2026-08-29 — first real consumer). Resuming
    from the newest stored point makes repeat cycles converge on
    now, exactly like the other venues' newest-first pages."""
    if lane.name == "binance-usdm":
        raw = http.get(
            f"https://{http.hosts['binance-usdm']}/fapi/v1/fundingRate"
            f"?symbol={target.instrument}&limit=1000"
        )
        return parse_bn_funding(raw) if raw is not None else None
    if lane.name == "okx":
        raw = http.get(
            f"https://{http.hosts['okx']}/api/v5/public/funding-rate-history"
            f"?instId={target.instrument}&limit=100"
        )
        return parse_okx_funding(raw) if raw is not None else None
    if lane.name == "deribit":
        start = now_ms - DERIBIT_RANGE_D * MS_1D
        raw = http.get(
            f"https://{http.hosts['deribit']}/api/v2/public/get_funding_rate_history"
            f"?instrument_name={target.instrument}&start_timestamp={start}"
            f"&end_timestamp={now_ms}"
        )
        return parse_deribit_funding(raw) if raw is not None else None
    if lane.name == "hyperliquid":
        start = now_ms - HL_RANGE_D * MS_1D
        if resume_ms is not None and resume_ms + 1 > start:
            start = resume_ms + 1
        body = json.dumps(
            {"type": "fundingHistory", "coin": target.instrument, "startTime": start},
            separators=(",", ":"),
        )
        raw = http.post(f"https://{http.hosts['hyperliquid']}/info", body)
        return parse_hl_funding(raw) if raw is not None else None
    if lane.name == "bybit-linear":
        raw = http.get(
            f"https://{http.hosts['bybit']}/v5/market/funding/history"
            f"?category=linear&symbol={target.instrument}&limit=200"
        )
        return parse_bybit_funding(raw) if raw is not None else None
    raise ValueError(f"funding: no fetcher for lane {lane.name}")


def run_cycle(
    conn: sqlite3.Connection,
    lanes: list[FundingLane],
    http: claude_worker.candles.Http,
    now_ms: int,
    budget_per_h: int,
    report: collections.abc.Callable[[str], None],
) -> None:
    """One newest-page cycle over every lane × target (per-venue
    budgets, the fetchers convention)."""
    budgets: dict[int, claude_worker.features.RestBudget] = {}
    for lane in lanes:
        budgets.setdefault(
            lane.venue,
            claude_worker.features.RestBudget(
                budget_per_h, claude_worker.fetchers.BUDGET_WINDOW_NS
            ),
        )
    for lane in lanes:
        budget = budgets[lane.venue]
        added = 0
        points_seen = 0
        failed = 0
        budget_out = False
        for target in lane.targets:
            if not budget.try_acquire():
                budget_out = True
                break
            resume_row = conn.execute(
                "SELECT MAX(ts_ms) FROM funding WHERE descriptor = ?",
                (target.descriptor,),
            ).fetchone()
            resume_ms = resume_row[0] if resume_row and resume_row[0] is not None else None
            points = _fetch_points(http, lane, target, now_ms, resume_ms)
            if points is None:
                failed += 1
                continue
            points_seen += len(points)
            added += upsert_points(conn, target.venue, target.descriptor, points, now_ms)
        conn.commit()
        report(
            f"funding: {lane.name}: targets={len(lane.targets)} points={points_seen}"
            f" +{added} failed={failed}{' BUDGET' if budget_out else ''}"
        )


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.funding")
    parser.add_argument("--universe", default=None)
    parser.add_argument("--db", default=None)
    parser.add_argument("--budget-per-h", type=int, default=None)
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    args = parser.parse_args(argv)
    env = os.environ
    universe = pathlib.Path(
        args.universe
        or env.get(claude_worker.fetchers.UNIVERSE_FILE_ENV, "")
        or claude_worker.candles.DEFAULT_UNIVERSE_PATH
    ).expanduser()
    db_path = pathlib.Path(
        args.db
        or env.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH
    ).expanduser()
    budget = args.budget_per_h or int(env.get(BUDGET_ENV, "") or BUDGET_PER_H_DEFAULT)
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
    lanes = read_funding_lanes(universe)
    if lanes is None:
        print(f"funding: unusable universe file {universe}", file=sys.stderr)
        return 1
    if not lanes:
        print(f"funding: no funding-bearing instruments in {universe}", file=sys.stderr)
        return 0
    conn = claude_worker.candles.open_db(db_path)
    try:
        ensure_schema(conn)
        with httpx.Client() as client:
            run_cycle(
                conn,
                lanes,
                claude_worker.candles.make_http(client, env),
                now_ms,
                budget,
                lambda line: print(line, file=sys.stderr),
            )
        total = conn.execute("SELECT count(*) FROM funding").fetchone()[0]
        stamp = datetime.datetime.fromtimestamp(now_ms / 1000, tz=datetime.timezone.utc)
        print(
            f"funding: cycle done {stamp.isoformat()} rows={total} db={db_path}",
            file=sys.stderr,
        )
    finally:
        conn.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
