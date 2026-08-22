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

import claude_worker.features
import claude_worker.fetchers
import claude_worker.frames

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
    """Open/create the store (WAL; §9.4 schema)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.executescript(_SCHEMA)
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
    usdm = [
        LaneTarget(frames.VENUE_BINANCE, f"binance-usdm:{s}", s.upper())
        for s in str_list("binance", "usdm")
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
    freshest window always fills first."""
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
        for target in lane.targets:
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
