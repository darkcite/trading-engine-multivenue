# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""candles — the §9.4–§9.6 persistent candle store (M3 C4).

A standalone MODULE (``python -m claude_worker.candles``) — NOT a
verb: the 7-verb CLI surface is FROZEN (``cli.py`` untouched). One
cycle = read the universe file → per (descriptor, timeframe): §9.6
gap-fill fetch under a RestBudget → upsert into the worker-owned
SQLite store. The M3 launchd hourly agent drives cycles; running by
hand any time is safe (idempotent by construction).

BINDING laws (docs/mvp-completion-plan.md §9, verbatim inheritance):

- **§9.4 store**: SQLite WAL, ONE table, PK ``(venue, descriptor,
  tf, open_ts)``. ``descriptor`` is the venue instrument string in
  the worker map-name convention (``binance:btcusdt``,
  ``binance-usdm:btcusdt``, ``okx:BTC-USDT``,
  ``deribit:BTC-PERPETUAL``, ``hyperliquid:BTC``, a PM token id) —
  NEVER a bare SymbolId (ids reshuffle across boots). Columns
  ``o,h,l,c,v``, ``source`` ∈ ``rest|derived|capture``,
  ``fetched_ts``.
- **§9.5 timeframe policy**: fetch ONLY the bases — 1m (rolling
  48 h), 1h (90 d), 1d (listing lifetime where the venue makes it
  cheap; OKX's backward 100-row pages make it expensive, so OKX 1d
  is bounded to 400 d — the §9.5 cheapness carve-out). 5m/15m/4h are
  NEVER fetched (derived exactly, C5).
- **§9.6 gap-fill**: per (descriptor, tf): ``SELECT max(open_ts)`` →
  request ONLY the missing window (re-requesting the stored max bar
  itself, which may have been OPEN at the last cycle) → paginate
  under the budget → upsert. Empty store ⇒ bounded backfill. Budget
  exhaustion mid-fill is fine — the next cycle resumes from
  ``max(open_ts)`` by construction. The still-OPEN bar is upserted
  until it closes; CLOSED ``rest`` bars are IMMUTABLE — a refetch
  disagreeing with a stored closed bar is LOGGED into
  ``candle_conflicts`` (market-map conflict pattern), never
  overwritten. Stored-closedness is judged from the row's OWN
  ``fetched_ts`` (``open_ts + tf_ms <= fetched_ts``): a bar stored
  mid-life is a snapshot that later fetches FINALIZE; only a bar
  that had already closed when fetched is immutable. A ``rest`` bar arriving on a
  PK held by a ``capture`` bar supersedes it (capture fills in only
  where no rest lane exists, §9.7); ``derived`` rows never share a
  fetched tf by the §9.5 split.
- **Pagination hole-avoidance**: forward lanes (Binance klines,
  Deribit chart, HL candleSnapshot) upsert page-by-page — progress
  is monotone from ``max(open_ts)``. The OKX lane pages BACKWARD
  (``history-candles`` + ``after``), so its walk is buffered and
  upserted ONLY when it connects to the stored frontier — a
  budget-truncated backward walk is discarded whole, otherwise
  ``max(open_ts)`` would leap over a permanent hole.

Best-effort discipline throughout (fetchers pattern): transport
failure / unusable body = counted + skipped, never a crash; the
cycle exits 0 with a per-lane stats report on stderr. No live API
calls in tests — HTTP rides injectable ``get_fn``/``post_fn``.
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
import tomllib
import typing

import httpx

import claude_worker.cli
import claude_worker.features
import claude_worker.fetchers
import claude_worker.frames
import claude_worker.pmlr

CANDLES_DB_ENV: str = "CLAUDE_WORKER_CANDLES_DB"
DEFAULT_DB_PATH: str = "~/multivenue/worker/candles.db"
DEFAULT_UNIVERSE_PATH: str = "~/multivenue/universe.toml"
BUDGET_ENV: str = "CLAUDE_WORKER_CANDLES_BUDGET_PER_H"
BUDGET_PER_H_DEFAULT: int = 30

# Binance REST hosts (the ONLY new host keys — the engine has no
# Binance REST lane to reuse; keyless public endpoints).
BN_REST_HOST_ENV: str = "BINANCE_REST_HOST"
BN_REST_HOST_DEFAULT: str = "api.binance.com"
BN_FUT_REST_HOST_ENV: str = "BINANCE_FUT_REST_HOST"
BN_FUT_REST_HOST_DEFAULT: str = "fapi.binance.com"
# WS9: Bybit REST host (mirrors the engine's BYBIT_REST_HOST).
BYBIT_REST_HOST_ENV: str = "BYBIT_REST_HOST"
BYBIT_REST_HOST_DEFAULT: str = "api.bybit.com"

MS_1M: int = 60_000
MS_1H: int = 3_600_000
MS_1D: int = 86_400_000

# §9.5 fetch bases: tf -> bar ms.
FETCHED_TFS: dict[str, int] = {"1m": MS_1M, "1h": MS_1H, "1d": MS_1D}

# §9.6 bounded-backfill horizons (config-tunable via env; ms; None =
# listing lifetime).
BACKFILL_1M_H_ENV: str = "CLAUDE_WORKER_CANDLES_BACKFILL_1M_H"
BACKFILL_1H_D_ENV: str = "CLAUDE_WORKER_CANDLES_BACKFILL_1H_D"
BACKFILL_1M_H_DEFAULT: int = 48
BACKFILL_1H_D_DEFAULT: int = 90
OKX_1D_BOUND_D: int = 400  # §9.5 cheapness carve-out, documented above
# Lifetime-1d floors for the forward windowed lanes (live-probed
# 2026-08-22): paging from epoch 0 in bar-count windows crawls the
# 1970s in 1000-day hops (Deribit answers no_data → abort), and HL
# rejects startTime=0 outright. Both venues CLAMP a pre-listing
# start to their earliest bar, so a pre-launch floor = true listing
# lifetime. Binance handles startTime=0 natively (floor stays 0).
DERIBIT_1D_FLOOR_MS: int = 1_451_606_400_000  # 2016-01-01, pre-launch
HL_1D_FLOOR_MS: int = 1_577_836_800_000  # 2020-01-01 (earliest observed: 2020-08)

SOURCE_REST: str = "rest"
SOURCE_DERIVED: str = "derived"
SOURCE_CAPTURE: str = "capture"

_SCHEMA: str = """
CREATE TABLE IF NOT EXISTS candles (
  venue      INTEGER NOT NULL,
  descriptor TEXT    NOT NULL,
  tf         TEXT    NOT NULL,
  open_ts    INTEGER NOT NULL,
  o REAL, h REAL, l REAL, c REAL,
  v REAL,
  n INTEGER,
  source     TEXT    NOT NULL CHECK (source IN ('rest','derived','capture')),
  fetched_ts INTEGER NOT NULL,
  PRIMARY KEY (venue, descriptor, tf, open_ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS candle_conflicts (
  venue      INTEGER NOT NULL,
  descriptor TEXT    NOT NULL,
  tf         TEXT    NOT NULL,
  open_ts    INTEGER NOT NULL,
  o REAL, h REAL, l REAL, c REAL,
  v REAL,
  first_seen_ts INTEGER NOT NULL,
  PRIMARY KEY (venue, descriptor, tf, open_ts)
) WITHOUT ROWID;
"""


class UpsertStats(typing.NamedTuple):
    """Per-upsert accounting (report surface)."""

    inserted: int
    updated_open: int
    conflicts: int
    unchanged: int
    superseded_capture: int


class LaneTarget(typing.NamedTuple):
    """One instrument in one lane."""

    venue: int
    descriptor: str
    instrument: str


class Lane(typing.NamedTuple):
    """One venue fetch lane (forward unless ``backward``)."""

    name: str
    venue: int
    targets: list[LaneTarget]
    backward: bool


# ---- store ---------------------------------------------------------------


def open_db(path: pathlib.Path) -> sqlite3.Connection:
    """Open/create the store (WAL; §9.4 schema + the §9.7 ``n``
    tick-count column — NULL except on capture bars)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.executescript(_SCHEMA)
    # C4→C5 migration: pre-`n` stores gain the column in place.
    cols = {row[1] for row in conn.execute("PRAGMA table_info(candles)")}
    if "n" not in cols:
        conn.execute("ALTER TABLE candles ADD COLUMN n INTEGER")
    return conn


def max_open_ts(conn: sqlite3.Connection, venue: int, descriptor: str, tf: str) -> int | None:
    """§9.6 frontier."""
    row = conn.execute(
        "SELECT max(open_ts) FROM candles WHERE venue=? AND descriptor=? AND tf=?",
        (venue, descriptor, tf),
    ).fetchone()
    return typing.cast(int | None, row[0]) if row is not None else None


def upsert_rest(
    conn: sqlite3.Connection,
    venue: int,
    descriptor: str,
    tf: str,
    tf_ms: int,
    bars: list[claude_worker.fetchers.Candle],
    now_ms: int,
) -> UpsertStats:
    """§9.6 upsert of one fetched page. Closed ``rest`` bars are
    immutable (disagreement → ``candle_conflicts``); the open bar
    updates in place; ``capture`` rows are superseded by rest."""
    inserted = 0
    updated_open = 0
    conflicts = 0
    unchanged = 0
    superseded = 0
    for bar in bars:
        row = conn.execute(
            "SELECT o,h,l,c,v,source,fetched_ts FROM candles"
            " WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?",
            (venue, descriptor, tf, bar.ts_ms),
        ).fetchone()
        if row is None:
            conn.execute(
                "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts)"
                " VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                (
                    venue,
                    descriptor,
                    tf,
                    bar.ts_ms,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.volume,
                    SOURCE_REST,
                    now_ms,
                ),
            )
            inserted += 1
            continue
        o, h, l, c, v, source, stored_fetched_ts = row
        same = (
            o == bar.open and h == bar.high and l == bar.low and c == bar.close and v == bar.volume
        )
        if source == SOURCE_REST and same:
            unchanged += 1
            continue
        # Final only if the stored row had CLOSED when it was fetched.
        closed_stored = bar.ts_ms + tf_ms <= stored_fetched_ts
        if source == SOURCE_REST and closed_stored:
            conn.execute(
                "INSERT OR REPLACE INTO candle_conflicts"
                " (venue,descriptor,tf,open_ts,o,h,l,c,v,first_seen_ts)"
                " VALUES (?,?,?,?,?,?,?,?,?,COALESCE("
                "   (SELECT first_seen_ts FROM candle_conflicts"
                "    WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?), ?))",
                (
                    venue,
                    descriptor,
                    tf,
                    bar.ts_ms,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.volume,
                    venue,
                    descriptor,
                    tf,
                    bar.ts_ms,
                    now_ms,
                ),
            )
            conflicts += 1
            continue
        conn.execute(
            "UPDATE candles SET o=?,h=?,l=?,c=?,v=?,source=?,fetched_ts=?"
            " WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?",
            (
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                bar.volume,
                SOURCE_REST,
                now_ms,
                venue,
                descriptor,
                tf,
                bar.ts_ms,
            ),
        )
        if source == SOURCE_CAPTURE:
            superseded += 1
        else:
            updated_open += 1
    conn.commit()
    return UpsertStats(inserted, updated_open, conflicts, unchanged, superseded)


# ---- universe → lanes ----------------------------------------------------


def read_universe_lanes(universe_path: pathlib.Path) -> list[Lane] | None:
    """Parse universe.toml into fetch lanes (map-name descriptor
    convention; PM deliberately absent — §9.7 capture lane, C5).
    ``None`` on unusable file (best-effort law)."""
    try:
        obj = tomllib.loads(universe_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError):
        return None

    def str_list(section: str, key: str) -> list[str]:
        sec = obj.get(section)
        if not isinstance(sec, dict):
            return []
        raw = sec.get(key)
        if not isinstance(raw, list):
            return []
        return [x for x in typing.cast(list[object], raw) if isinstance(x, str) and x]

    frames = claude_worker.frames
    lanes: list[Lane] = []
    spot = [
        LaneTarget(frames.VENUE_BINANCE, f"binance:{s}", s.upper())
        for s in str_list("binance", "spot")
    ]
    if spot:
        lanes.append(Lane("binance", frames.VENUE_BINANCE, spot, backward=False))
    # WS5: dated delivery futures ride the SAME fapi lane + descriptor
    # namespace as the perps (klines/ticker endpoints serve both).
    usdm = [
        LaneTarget(frames.VENUE_BINANCE, f"binance-usdm:{s}", s.upper())
        for s in str_list("binance", "usdm") + str_list("binance", "usdm_dated")
    ]
    if usdm:
        lanes.append(Lane("binance-usdm", frames.VENUE_BINANCE, usdm, backward=False))
    okx = [
        LaneTarget(frames.VENUE_OKX, f"okx:{i}", i) for i in str_list("okx", "instruments")
    ]
    if okx:
        lanes.append(Lane("okx", frames.VENUE_OKX, okx, backward=True))
    deribit = [
        LaneTarget(frames.VENUE_DERIBIT, f"deribit:{i}", i)
        for i in str_list("deribit", "instruments")
    ]
    if deribit:
        lanes.append(Lane("deribit", frames.VENUE_DERIBIT, deribit, backward=False))
    hl = [
        LaneTarget(frames.VENUE_HYPERLIQUID, f"hyperliquid:{c}", c)
        for c in str_list("hyperliquid", "coins")
    ]
    if hl:
        lanes.append(Lane("hyperliquid", frames.VENUE_HYPERLIQUID, hl, backward=False))
    # WS9: Bybit — one lane per category (the kline endpoint needs
    # `category=`); symbols stay in the venue's UPPERCASE form.
    bybit_spot = [
        LaneTarget(frames.VENUE_BYBIT, f"bybit:{s}", s) for s in str_list("bybit", "spot")
    ]
    if bybit_spot:
        lanes.append(Lane("bybit", frames.VENUE_BYBIT, bybit_spot, backward=False))
    bybit_linear = [
        LaneTarget(frames.VENUE_BYBIT, f"bybit-linear:{s}", s)
        for s in str_list("bybit", "linear")
    ]
    if bybit_linear:
        lanes.append(Lane("bybit-linear", frames.VENUE_BYBIT, bybit_linear, backward=False))
    return lanes


# ---- venue request/parse adapters ---------------------------------------

BN_PAGE_LIMIT: int = 1000
DERIBIT_PAGE_BARS: int = 1000
HL_PAGE_BARS: int = 1000
OKX_PAGE_LIMIT: int = 100

# Venue interval grammars.
BN_INTERVAL: dict[str, str] = {"1m": "1m", "1h": "1h", "1d": "1d"}
OKX_BAR: dict[str, str] = {"1m": "1m", "1h": "1H", "1d": "1Dutc"}
DERIBIT_RESOLUTION: dict[str, str] = {"1m": "1", "1h": "60", "1d": "1D"}
HL_INTERVAL: dict[str, str] = {"1m": "1m", "1h": "1h", "1d": "1d"}
# WS9: Bybit kline intervals.
BYBIT_INTERVAL: dict[str, str] = {"1m": "1", "1h": "60", "1d": "D"}
BYBIT_PAGE_BARS: int = 1000
# WS9: pre-launch 1d floor (linear launched 2019; spot 2021 — both
# clamp a pre-listing start to their earliest bar, the Deribit/HL
# pattern).
BYBIT_1D_FLOOR_MS: int = 1_514_764_800_000  # 2018-01-01


def parse_binance_klines(raw: str) -> tuple[list[claude_worker.fetchers.Candle], int] | None:
    """Strict parse of Binance ``/klines`` (spot and USDS-M share the
    shape): array of arrays ``[openTime, o, h, l, c, v, …]`` with
    string prices. ``None`` = unusable body; malformed rows counted
    (labeling.py discipline: skip, never crash)."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, list):
        return None
    out: list[claude_worker.fetchers.Candle] = []
    malformed = 0
    for row in typing.cast(list[object], obj):
        if not isinstance(row, list) or len(row) < 6:
            malformed += 1
            continue
        cells = typing.cast(list[object], row)
        ts = cells[0]
        if isinstance(ts, bool) or not isinstance(ts, int):
            malformed += 1
            continue
        vals: list[float] = []
        ok = True
        for cell in cells[1:6]:
            if not isinstance(cell, str):
                ok = False
                break
            try:
                vals.append(float(cell))
            except ValueError:
                ok = False
                break
        if not ok:
            malformed += 1
            continue
        out.append(
            claude_worker.fetchers.Candle(
                ts_ms=ts, open=vals[0], high=vals[1], low=vals[2], close=vals[3], volume=vals[4]
            )
        )
    out.sort(key=lambda candle: candle.ts_ms)
    return out, malformed


def parse_bybit_kline(raw: str) -> tuple[list[claude_worker.fetchers.Candle], int] | None:
    """WS9: strict parse of Bybit ``/v5/market/kline`` (spot and
    linear share the shape): ``result.list`` rows of STRINGS
    ``[startMs, o, h, l, c, volume, turnover]``, NEWEST-first on the
    wire — normalized ascending here. ``None`` = unusable body
    (including ``retCode != 0``); malformed rows counted."""
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
    out: list[claude_worker.fetchers.Candle] = []
    malformed = 0
    for row in typing.cast(list[object], rows):
        if not isinstance(row, list) or len(row) < 6:
            malformed += 1
            continue
        cells = typing.cast(list[object], row)
        ok = True
        vals: list[float] = []
        ts = 0
        for k, cell in enumerate(cells[:6]):
            if not isinstance(cell, str):
                ok = False
                break
            try:
                if k == 0:
                    ts = int(cell)
                else:
                    vals.append(float(cell))
            except ValueError:
                ok = False
                break
        if not ok:
            malformed += 1
            continue
        out.append(
            claude_worker.fetchers.Candle(
                ts_ms=ts, open=vals[0], high=vals[1], low=vals[2], close=vals[3], volume=vals[4]
            )
        )
    out.sort(key=lambda candle: candle.ts_ms)
    return out, malformed


def _bybit_url(host: str, category: str, symbol: str, tf: str, start_ms: int) -> str:
    return (
        f"https://{host}/v5/market/kline?category={category}&symbol={symbol}"
        f"&interval={BYBIT_INTERVAL[tf]}&start={start_ms}&limit={BYBIT_PAGE_BARS}"
    )


def _bn_url(host: str, path: str, symbol: str, tf: str, start_ms: int) -> str:
    return (
        f"https://{host}{path}?symbol={symbol}&interval={BN_INTERVAL[tf]}"
        f"&startTime={start_ms}&limit={BN_PAGE_LIMIT}"
    )


def _okx_history_url(host: str, inst: str, tf: str, after_ms: int) -> str:
    return (
        f"https://{host}/api/v5/market/history-candles?instId={inst}"
        f"&bar={OKX_BAR[tf]}&after={after_ms}&limit={OKX_PAGE_LIMIT}"
    )


def _deribit_url(host: str, inst: str, tf: str, start_ms: int, end_ms: int) -> str:
    return (
        f"https://{host}/api/v2/public/get_tradingview_chart_data"
        f"?instrument_name={inst}&start_timestamp={start_ms}"
        f"&end_timestamp={end_ms}&resolution={DERIBIT_RESOLUTION[tf]}"
    )


class Http(typing.NamedTuple):
    """Injectable transport pair (no live calls in tests)."""

    get: collections.abc.Callable[[str], str | None]
    post: collections.abc.Callable[[str, str], str | None]
    hosts: dict[str, str]


def _fetch_forward_page(
    http: Http, lane: Lane, target: LaneTarget, tf: str, lo_ms: int, now_ms: int
) -> list[claude_worker.fetchers.Candle] | None:
    """One forward page ``[lo, …]`` for the forward lanes. ``None`` =
    transport/parse failure (skip target this cycle)."""
    tf_ms = FETCHED_TFS[tf]
    if lane.name == "binance":
        raw = http.get(_bn_url(http.hosts["binance"], "/api/v3/klines", target.instrument, tf, lo_ms))
        if raw is None:
            return None
        parsed = parse_binance_klines(raw)
        return None if parsed is None else parsed[0]
    if lane.name == "binance-usdm":
        raw = http.get(
            _bn_url(http.hosts["binance-usdm"], "/fapi/v1/klines", target.instrument, tf, lo_ms)
        )
        if raw is None:
            return None
        parsed = parse_binance_klines(raw)
        return None if parsed is None else parsed[0]
    if lane.name == "deribit":
        end = min(now_ms, lo_ms + DERIBIT_PAGE_BARS * tf_ms)
        raw = http.get(_deribit_url(http.hosts["deribit"], target.instrument, tf, lo_ms, end))
        if raw is None:
            return None
        return claude_worker.fetchers.parse_deribit_chart(raw)
    if lane.name == "bybit" or lane.name == "bybit-linear":
        # WS9: forward-paged; the category rides the lane name.
        category = "spot" if lane.name == "bybit" else "linear"
        raw = http.get(_bybit_url(http.hosts["bybit"], category, target.instrument, tf, lo_ms))
        if raw is None:
            return None
        parsed_bb = parse_bybit_kline(raw)
        return None if parsed_bb is None else parsed_bb[0]
    if lane.name == "hyperliquid":
        end = min(now_ms, lo_ms + HL_PAGE_BARS * tf_ms)
        body = json.dumps(
            {
                "type": "candleSnapshot",
                "req": {
                    "coin": target.instrument,
                    "interval": HL_INTERVAL[tf],
                    "startTime": lo_ms,
                    "endTime": end,
                },
            },
            separators=(",", ":"),
        )
        raw = http.post(f"https://{http.hosts['hyperliquid']}/info", body)
        if raw is None:
            return None
        parsed_hl = claude_worker.fetchers.parse_hl_candles(raw)
        return None if parsed_hl is None else parsed_hl[0]
    raise ValueError(f"not a forward lane: {lane.name}")


# ---- §9.6 gap-fill -------------------------------------------------------


def backfill_start_ms(tf: str, now_ms: int, lane_name: str, env: collections.abc.Mapping[str, str]) -> int:
    """Empty-store §9.6 backfill bound for one tf."""
    if tf == "1m":
        hours = int(env.get(BACKFILL_1M_H_ENV, "") or BACKFILL_1M_H_DEFAULT)
        return now_ms - hours * MS_1H
    if tf == "1h":
        days = int(env.get(BACKFILL_1H_D_ENV, "") or BACKFILL_1H_D_DEFAULT)
        return now_ms - days * MS_1D
    if lane_name == "okx":
        return now_ms - OKX_1D_BOUND_D * MS_1D  # §9.5 cheapness carve-out
    if lane_name == "deribit":
        return DERIBIT_1D_FLOOR_MS
    if lane_name == "hyperliquid":
        return HL_1D_FLOOR_MS
    if lane_name in ("bybit", "bybit-linear"):
        return BYBIT_1D_FLOOR_MS  # WS9: pre-launch floor (untested vs start=0)
    return 0  # Binance: startTime=0 = true listing lifetime (proven)


class FillStats(typing.NamedTuple):
    """Per-(descriptor, tf) fill accounting."""

    pages: int
    bars: int
    upsert: UpsertStats
    budget_out: bool
    failed: bool


_ZERO_UPSERT = UpsertStats(0, 0, 0, 0, 0)


def _merge(a: UpsertStats, b: UpsertStats) -> UpsertStats:
    return UpsertStats(
        a.inserted + b.inserted,
        a.updated_open + b.updated_open,
        a.conflicts + b.conflicts,
        a.unchanged + b.unchanged,
        a.superseded_capture + b.superseded_capture,
    )


def fill_forward(
    conn: sqlite3.Connection,
    http: Http,
    lane: Lane,
    target: LaneTarget,
    tf: str,
    now_ms: int,
    budget: claude_worker.features.RestBudget,
    env: collections.abc.Mapping[str, str],
) -> FillStats:
    """Forward §9.6 fill: page-by-page upsert, monotone frontier."""
    tf_ms = FETCHED_TFS[tf]
    last = max_open_ts(conn, target.venue, target.descriptor, tf)
    lo = last if last is not None else backfill_start_ms(tf, now_ms, lane.name, env)
    pages = 0
    bars = 0
    stats = _ZERO_UPSERT
    while lo <= now_ms:
        if not budget.try_acquire():
            return FillStats(pages, bars, stats, budget_out=True, failed=False)
        page = _fetch_forward_page(http, lane, target, tf, lo, now_ms)
        if page is None:
            return FillStats(pages, bars, stats, budget_out=False, failed=True)
        pages += 1
        if not page:
            break
        bars += len(page)
        stats = _merge(stats, upsert_rest(conn, target.venue, target.descriptor, tf, tf_ms, page, now_ms))
        new_lo = page[-1].ts_ms + tf_ms
        if new_lo <= lo:
            break  # progress guard: a stuck venue never loops us
        lo = new_lo
        if page[-1].ts_ms + tf_ms > now_ms:
            break  # reached the open bar
    return FillStats(pages, bars, stats, budget_out=False, failed=False)


def fill_okx_backward(
    conn: sqlite3.Connection,
    http: Http,
    target: LaneTarget,
    tf: str,
    now_ms: int,
    budget: claude_worker.features.RestBudget,
    env: collections.abc.Mapping[str, str],
) -> FillStats:
    """OKX §9.6 fill: BACKWARD walk (``history-candles`` + ``after``),
    buffered, upserted only when the walk CONNECTS to the stored
    frontier (hole-avoidance law in the module docs)."""
    tf_ms = FETCHED_TFS[tf]
    last = max_open_ts(conn, target.venue, target.descriptor, tf)
    bound = last if last is not None else backfill_start_ms(tf, now_ms, "okx", env)
    buffered: list[claude_worker.fetchers.Candle] = []
    after = now_ms + tf_ms  # strictly newer than any bar we want
    pages = 0
    while True:
        if not budget.try_acquire():
            return FillStats(pages, 0, _ZERO_UPSERT, budget_out=True, failed=False)
        raw = http.get(_okx_history_url(http.hosts["okx"], target.instrument, tf, after))
        if raw is None:
            return FillStats(pages, 0, _ZERO_UPSERT, budget_out=False, failed=True)
        parsed = claude_worker.fetchers.parse_okx_candles(raw)
        if parsed is None:
            return FillStats(pages, 0, _ZERO_UPSERT, budget_out=False, failed=True)
        page, _malformed = parsed  # oldest-first normalized
        pages += 1
        if not page:
            break  # venue exhausted (listing edge)
        buffered = page + buffered
        oldest = page[0].ts_ms
        if oldest <= bound:
            break  # connected to the frontier / bound
        after = oldest  # next page: strictly older than this
        if len(page) < OKX_PAGE_LIMIT:
            break  # short page = no more history
    keep = [candle for candle in buffered if candle.ts_ms >= bound]
    stats = (
        upsert_rest(conn, target.venue, target.descriptor, tf, tf_ms, keep, now_ms)
        if keep
        else _ZERO_UPSERT
    )
    return FillStats(pages, len(keep), stats, budget_out=False, failed=False)


# ---- §9.5 derive + §9.7 capture lanes (C5) ------------------------------

# (tf_out, tf_base, bars_per_window, window_ms) — derived EXACTLY:
# O = first, H = max, L = min, C = last, V = sum (NULL-poisoning: any
# NULL base volume ⇒ NULL — volume is never fabricated, §9.7).
DERIVED_TFS: tuple[tuple[str, str, int, int], ...] = (
    ("5m", "1m", 5, 5 * MS_1M),
    ("15m", "1m", 15, 15 * MS_1M),
    ("4h", "1h", 4, 4 * MS_1H),
)

CAPTURE_WINDOW_H_ENV: str = "CLAUDE_WORKER_CANDLES_CAPTURE_WINDOW_H"
CAPTURE_WINDOW_H_DEFAULT: int = 26  # daily-restart run + margin
DRIFT_WINDOW_H_ENV: str = "CLAUDE_WORKER_CANDLES_DRIFT_WINDOW_H"
DRIFT_WINDOW_H_DEFAULT: int = 6
DRIFT_WARN_BPS_ENV: str = "CLAUDE_WORKER_CANDLES_DRIFT_WARN_BPS"
DRIFT_WARN_BPS_DEFAULT: float = 20.0


def derive_pass(
    conn: sqlite3.Connection,
    now_ms: int,
    report: collections.abc.Callable[[str], None],
) -> None:
    """§9.5: recompute 5m/15m/4h from the stored finer bases (rest +
    capture rows), store back ``source=derived``. Only COMPLETE,
    CLOSED windows (all ``k`` base bars present, window end ≤ now) —
    a window still missing base bars materializes once §9.6 fills
    them. Derived rows are a CACHE: they refresh freely when a base
    finalization changes them (immutability protects fetched rest
    bars only). Bounded by construction: 1m bases are rolling 48 h
    (+ PM capture), 1h bases rolling 90 d."""
    for tf_out, tf_base, k, ms_out in DERIVED_TFS:
        rows = conn.execute(
            "SELECT venue, descriptor, open_ts, o, h, l, c, v FROM candles"
            " WHERE tf=? AND source IN ('rest','capture')"
            " ORDER BY venue, descriptor, open_ts",
            (tf_base,),
        ).fetchall()
        made = 0
        refreshed = 0
        unchanged = 0
        bucket: list[tuple[int, float, float, float, float, float | None]] = []
        cur: tuple[int, str, int] | None = None  # (venue, descriptor, window)

        def flush() -> None:
            nonlocal made, refreshed, unchanged
            if cur is None or len(bucket) != k:
                return
            venue, descriptor, window = cur
            if window + ms_out > now_ms:
                return  # window still open
            o = bucket[0][1]
            h = max(b[2] for b in bucket)
            low = min(b[3] for b in bucket)
            c = bucket[-1][4]
            vols = [b[5] for b in bucket]
            v = None if any(x is None for x in vols) else sum(typing.cast(list[float], vols))
            row = conn.execute(
                "SELECT o,h,l,c,v,source FROM candles"
                " WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?",
                (venue, descriptor, tf_out, window),
            ).fetchone()
            if row is None:
                conn.execute(
                    "INSERT INTO candles"
                    " (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts)"
                    " VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                    (venue, descriptor, tf_out, window, o, h, low, c, v, SOURCE_DERIVED, now_ms),
                )
                made += 1
                return
            if row[5] != SOURCE_DERIVED:
                return  # never touch a non-derived row (tf spaces disjoint by law)
            if (row[0], row[1], row[2], row[3], row[4]) == (o, h, low, c, v):
                unchanged += 1
                return
            conn.execute(
                "UPDATE candles SET o=?,h=?,l=?,c=?,v=?,fetched_ts=?"
                " WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?",
                (o, h, low, c, v, now_ms, venue, descriptor, tf_out, window),
            )
            refreshed += 1

        for venue, descriptor, open_ts, o, h, low, c, v in rows:
            window = open_ts - (open_ts % ms_out)
            key = (venue, descriptor, window)
            if key != cur:
                flush()
                cur = key
                bucket = []
            bucket.append((open_ts, o, h, low, c, v))
        flush()
        conn.commit()
        report(
            f"candles: derive {tf_out}: +{made} refreshed={refreshed} unchanged={unchanged}"
        )


class MinuteBar(typing.NamedTuple):
    """One folded capture minute (§9.7): mid-price OHLC + tick count."""

    o: float
    h: float
    low: float
    c: float
    n: int


# WS11 (D5): the anchor folded into pmlr.py; alias keeps the name.
_run_anchor_ns = claude_worker.pmlr.run_anchor_ns


def fold_capture_minutes(
    replay_root: pathlib.Path,
    sym_to_desc: dict[int, str],
    venue_labels: tuple[str, ...],
    lo_ms: int,
    now_ms: int,
) -> dict[tuple[str, int], MinuteBar]:
    """Walk the capture (runs overlapping ``[lo, now]``, given venue
    tick files only) → per (descriptor, minute) mid-price OHLC +
    tick-count. Wall mapping is the harness law:
    ``wall = epoch_ns + (ts − run_anchor)``. One-sided ticks (a zero
    bid or ask) are skipped — a fabricated mid is worse than a gap."""
    out: dict[tuple[str, int], MinuteBar] = {}
    for run_dir in claude_worker.features.run_dirs(replay_root):
        try:
            epoch_ns = int(run_dir.name[len("run-") :])
        except ValueError:
            continue
        # A run spans at most ~a day (daily restart); cheap skip for
        # runs that cannot reach the window.
        if epoch_ns // 1_000_000 + 36 * MS_1H < lo_ms:
            continue
        anchor = _run_anchor_ns(run_dir)
        if anchor is None:
            continue
        for label in venue_labels:
            path = run_dir / f"{label}-ticks.pmlr"
            if not path.is_file():
                continue
            try:
                with claude_worker.pmlr.Reader(path) as reader:
                    if (
                        reader.slot_kind != claude_worker.pmlr.SLOT_KIND_TICK
                        or len(reader) == 0
                    ):
                        continue
                    for tick in reader.ticks():
                        desc = sym_to_desc.get(tick.sym)
                        if desc is None or tick.bid_px <= 0 or tick.ask_px <= 0:
                            continue
                        wall_ms = (epoch_ns + (tick.ts_ns - anchor)) // 1_000_000
                        if wall_ms < lo_ms or wall_ms >= now_ms:
                            continue
                        minute = wall_ms - (wall_ms % MS_1M)
                        mid = tick.mid() / 1_000_000
                        key = (desc, minute)
                        bar = out.get(key)
                        if bar is None:
                            out[key] = MinuteBar(mid, mid, mid, mid, 1)
                        else:
                            out[key] = MinuteBar(
                                bar.o,
                                mid if mid > bar.h else bar.h,
                                mid if mid < bar.low else bar.low,
                                mid,
                                bar.n + 1,
                            )
            except (claude_worker.pmlr.PmlrError, OSError, ValueError):
                continue
    return out


def upsert_capture(
    conn: sqlite3.Connection,
    venue: int,
    bars: dict[tuple[str, int], MinuteBar],
    now_ms: int,
) -> tuple[int, int, int]:
    """§9.7 store lane: capture bars land only where no REST row holds
    the PK (rest supersedes capture, never the reverse); existing
    capture rows refresh when the fold changed them (a still-filling
    minute at the previous cycle). ``v`` stays NULL — we capture BBO,
    volume is never fabricated. Returns (inserted, refreshed,
    rest_kept)."""
    inserted = 0
    refreshed = 0
    rest_kept = 0
    for (descriptor, minute), bar in sorted(bars.items()):
        row = conn.execute(
            "SELECT o,h,l,c,n,source FROM candles"
            " WHERE venue=? AND descriptor=? AND tf='1m' AND open_ts=?",
            (venue, descriptor, minute),
        ).fetchone()
        if row is None:
            conn.execute(
                "INSERT INTO candles"
                " (venue,descriptor,tf,open_ts,o,h,l,c,v,n,source,fetched_ts)"
                " VALUES (?,?,'1m',?,?,?,?,?,NULL,?,?,?)",
                (venue, descriptor, minute, bar.o, bar.h, bar.low, bar.c, bar.n, SOURCE_CAPTURE, now_ms),
            )
            inserted += 1
            continue
        if row[5] != SOURCE_CAPTURE:
            rest_kept += 1
            continue
        if (row[0], row[1], row[2], row[3], row[4]) == (bar.o, bar.h, bar.low, bar.c, bar.n):
            continue
        conn.execute(
            "UPDATE candles SET o=?,h=?,l=?,c=?,n=?,fetched_ts=?"
            " WHERE venue=? AND descriptor=? AND tf='1m' AND open_ts=?",
            (bar.o, bar.h, bar.low, bar.c, bar.n, now_ms, venue, descriptor, minute),
        )
        refreshed += 1
    conn.commit()
    return inserted, refreshed, rest_kept


def drift_check(
    conn: sqlite3.Connection,
    venue: int,
    capture: dict[tuple[str, int], MinuteBar],
    warn_bps: float,
    report: collections.abc.Callable[[str], None],
) -> None:
    """§9.7 job (2): REST candles cross-checked against what our own
    sockets saw — close-vs-close in bps over the overlapping minutes.
    Report-only; a WARN line when the max drift crosses the
    threshold."""
    per_desc: dict[str, list[float]] = {}
    for (descriptor, minute), bar in capture.items():
        row = conn.execute(
            "SELECT c FROM candles"
            " WHERE venue=? AND descriptor=? AND tf='1m' AND open_ts=? AND source='rest'",
            (venue, descriptor, minute),
        ).fetchone()
        if row is None or row[0] is None or row[0] == 0:
            continue
        rest_close = typing.cast(float, row[0])
        bps = abs(rest_close - bar.c) / rest_close * 10_000.0
        per_desc.setdefault(descriptor, []).append(bps)
    for descriptor in sorted(per_desc):
        samples = per_desc[descriptor]
        worst = max(samples)
        mean = sum(samples) / len(samples)
        flag = " WARN" if worst > warn_bps else ""
        report(
            f"candles: drift {descriptor}: minutes={len(samples)}"
            f" mean={mean:.2f}bps max={worst:.2f}bps{flag}"
        )


def sym_maps(markets: dict[str, int]) -> tuple[dict[int, str], dict[int, str]]:
    """Invert the worker market map for the §9.7 lanes: PM syms →
    token-id descriptor (the all-digit map name, §9.4), BN syms →
    ``binance:*`` spot descriptor (drift lane). Sorted iteration ⇒
    deterministic pick when a sym carries several names."""
    pm: dict[int, str] = {}
    bn: dict[int, str] = {}
    for name in sorted(markets):
        sym = markets[name]
        if name.isdigit() and PM_TOKEN_RUN_MIN <= len(name) <= PM_TOKEN_MAX:
            pm.setdefault(sym, name)
        elif name.startswith("binance:"):
            bn.setdefault(sym, name)
    return pm, bn


PM_TOKEN_RUN_MIN: int = claude_worker.fetchers.PM_TOKEN_RUN_MIN
PM_TOKEN_MAX: int = claude_worker.fetchers.PM_TOKEN_MAX


def capture_and_derive(
    conn: sqlite3.Connection,
    replay_root: pathlib.Path,
    markets: dict[str, int],
    now_ms: int,
    env: collections.abc.Mapping[str, str],
    report: collections.abc.Callable[[str], None],
    capture_backfill: bool = False,
) -> None:
    """The C5 tail of one cycle: PM capture store lane → BN drift
    check → §9.5 derive pass. Missing replay root / empty map =
    reported skip, never an error (best-effort law)."""
    if not replay_root.is_dir():
        report(f"candles: capture lane skipped — no replay root {replay_root}")
        derive_pass(conn, now_ms, report)
        return
    pm_map, bn_map = sym_maps(markets)
    cap_h = int(env.get(CAPTURE_WINDOW_H_ENV, "") or CAPTURE_WINDOW_H_DEFAULT)
    drift_h = int(env.get(DRIFT_WINDOW_H_ENV, "") or DRIFT_WINDOW_H_DEFAULT)
    warn_bps = float(env.get(DRIFT_WARN_BPS_ENV, "") or DRIFT_WARN_BPS_DEFAULT)
    if pm_map:
        pm_lo = 0 if capture_backfill else now_ms - cap_h * MS_1H
        pm_bars = fold_capture_minutes(replay_root, pm_map, ("pm",), pm_lo, now_ms)
        ins, ref, kept = upsert_capture(
            conn, claude_worker.frames.VENUE_POLYMARKET, pm_bars, now_ms
        )
        report(
            f"candles: capture pm: minutes={len(pm_bars)} +{ins} refreshed={ref}"
            f" rest_kept={kept}"
        )
    else:
        report("candles: capture pm: no PM token names in the market map — skipped")
    if bn_map:
        bn_bars = fold_capture_minutes(
            replay_root, bn_map, ("bn",), now_ms - drift_h * MS_1H, now_ms
        )
        drift_check(conn, claude_worker.frames.VENUE_BINANCE, bn_bars, warn_bps, report)
    derive_pass(conn, now_ms, report)


# ---- one cycle -----------------------------------------------------------


def run_cycle(
    conn: sqlite3.Connection,
    lanes: list[Lane],
    http: Http,
    now_ms: int,
    budget_per_h: int,
    env: collections.abc.Mapping[str, str],
    report: collections.abc.Callable[[str], None],
) -> None:
    """One §9.6 cycle over every lane × target × base tf. Budgets are
    per VENUE (fetchers convention); tf order 1m → 1h → 1d so the
    freshest window always fills first. Targets ROTATE by cycle hour:
    a backward lane whose walk exceeds the leftover budget would
    otherwise be discarded every cycle behind the same earlier
    siblings (observed live 2026-08-22: okx ETH-USDT-SWAP's 29-page
    1m walk vs 27 remaining) — rotation lets every target lead a
    cycle eventually, so every backfill completes."""
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
        rot = (now_ms // 3_600_000) % len(lane.targets) if lane.targets else 0
        rotated = lane.targets[rot:] + lane.targets[:rot]
        for target in rotated:
            for tf in FETCHED_TFS:
                if lane.backward:
                    st = fill_okx_backward(conn, http, target, tf, now_ms, budget, env)
                else:
                    st = fill_forward(conn, http, lane, target, tf, now_ms, budget, env)
                u = st.upsert
                report(
                    f"candles: {target.descriptor} {tf}: pages={st.pages} bars={st.bars}"
                    f" +{u.inserted} open~{u.updated_open} conflicts={u.conflicts}"
                    f" unchanged={u.unchanged} capture->rest={u.superseded_capture}"
                    f"{' BUDGET' if st.budget_out else ''}{' FAILED' if st.failed else ''}"
                )


def make_http(client: httpx.Client, env: collections.abc.Mapping[str, str]) -> Http:
    """The worker-standard transport pair over one httpx client."""

    def get(url: str) -> str | None:
        try:
            response = client.get(url, timeout=claude_worker.fetchers.REST_TIMEOUT_S)
        except httpx.HTTPError:
            return None
        if response.status_code != httpx.codes.OK:
            return None
        return response.text

    def post(url: str, body: str) -> str | None:
        try:
            response = client.post(
                url,
                content=body,
                headers={"Content-Type": "application/json"},
                timeout=claude_worker.fetchers.REST_TIMEOUT_S,
            )
        except httpx.HTTPError:
            return None
        if response.status_code != httpx.codes.OK:
            return None
        return response.text

    hosts = {
        "binance": env.get(BN_REST_HOST_ENV, "") or BN_REST_HOST_DEFAULT,
        "binance-usdm": env.get(BN_FUT_REST_HOST_ENV, "") or BN_FUT_REST_HOST_DEFAULT,
        "bybit": env.get(BYBIT_REST_HOST_ENV, "") or BYBIT_REST_HOST_DEFAULT,
        "okx": env.get(claude_worker.fetchers.OKX_REST_HOST_ENV, "")
        or claude_worker.fetchers.OKX_REST_HOST_DEFAULT,
        "deribit": env.get(claude_worker.fetchers.DERIBIT_REST_HOST_ENV, "")
        or claude_worker.fetchers.DERIBIT_REST_HOST_DEFAULT,
        "hyperliquid": env.get(claude_worker.fetchers.HL_API_HOST_ENV, "")
        or claude_worker.fetchers.HL_API_HOST_DEFAULT,
    }
    return Http(get=get, post=post, hosts=hosts)


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.candles")
    parser.add_argument("--universe", default=None)
    parser.add_argument("--db", default=None)
    parser.add_argument("--budget-per-h", type=int, default=None)
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    parser.add_argument(
        "--capture-backfill",
        action="store_true",
        help="one-shot: fold PM capture minutes over the WHOLE replay"
        " root, not just the rolling window (operator-invoked)",
    )
    args = parser.parse_args(argv)
    env = os.environ
    universe = pathlib.Path(
        args.universe
        or env.get(claude_worker.fetchers.UNIVERSE_FILE_ENV, "")
        or DEFAULT_UNIVERSE_PATH
    ).expanduser()
    db_path = pathlib.Path(args.db or env.get(CANDLES_DB_ENV, "") or DEFAULT_DB_PATH).expanduser()
    budget = args.budget_per_h or int(env.get(BUDGET_ENV, "") or BUDGET_PER_H_DEFAULT)
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
    lanes = read_universe_lanes(universe)
    if lanes is None:
        print(f"candles: unusable universe file {universe}", file=sys.stderr)
        return 1
    if not lanes:
        print(f"candles: no candle-lane instruments in {universe}", file=sys.stderr)
        return 0
    conn = open_db(db_path)
    try:
        with httpx.Client() as client:
            run_cycle(
                conn,
                lanes,
                make_http(client, env),
                now_ms,
                budget,
                env,
                lambda line: print(line, file=sys.stderr),
            )
        # C5 tail: PM capture store + BN drift + §9.5 derive.
        replay_root = pathlib.Path(
            env.get("CLAUDE_WORKER_REPLAY_DIR", "") or "~/multivenue/logs"
        ).expanduser()
        map_path = pathlib.Path(
            env.get("CLAUDE_WORKER_MARKET_MAP", "") or "~/multivenue/worker/market-map.json"
        ).expanduser()
        try:
            markets = claude_worker.cli.load_market_map(map_path).markets
        except ValueError as e:
            print(f"candles: market map unusable ({e}) — capture lane skipped", file=sys.stderr)
            markets = {}
        capture_and_derive(
            conn,
            replay_root,
            markets,
            now_ms,
            env,
            lambda line: print(line, file=sys.stderr),
            capture_backfill=args.capture_backfill,
        )
        total = conn.execute("SELECT count(*) FROM candles").fetchone()[0]
        conflicts = conn.execute("SELECT count(*) FROM candle_conflicts").fetchone()[0]
        stamp = datetime.datetime.fromtimestamp(now_ms / 1000, tz=datetime.timezone.utc)
        print(
            f"candles: cycle done {stamp.isoformat()} rows={total} conflict-rows={conflicts}"
            f" db={db_path}",
            file=sys.stderr,
        )
    finally:
        conn.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
