# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""refdata — the WS4 periodic reference-data lane (24h quote volume +
open interest snapshots).

A standalone MODULE (``python -m claude_worker.refdata``) — NOT a
verb: the verb CLI surface stays frozen (``cli.py`` untouched), the
iv_digest/candles precedent. One cycle = read the universe file →
per (venue, instrument): fetch the venue's 24h-volume / OI REST
snapshot under a per-venue ``RestBudget`` → upsert into the
``refdata`` table BESIDE candles inside ``candles.db``.

Placement law (stage2-finish-plan WS4): static metadata (tick/lot)
belongs to ENGINE boot discovery; periodic series (24h volume, OI)
belong to THIS worker lane — serialized, budgeted, stored keyed
``venue+descriptor`` (§9 keying; descriptors are the map-name
convention, NEVER bare SymbolIds). The engine's hot path never does
REST.

Venue scope (WS4 built the lane + BN/Deribit; WS7 added OKX; WS8
added Hyperliquid):

- ``binance`` (spot):    ``GET /api/v3/ticker/24hr?symbol=S``
  → ``quoteVolume`` (quote-ccy units) → kind ``vol24h_quote``.
- ``binance-usdm``:      ``GET /fapi/v1/ticker/24hr?symbol=S``
  → ``quoteVolume`` → ``vol24h_quote``; plus
  ``GET /fapi/v1/openInterest?symbol=S`` → ``openInterest``
  (BASE-asset units) → kind ``oi``.
- ``deribit``: ``GET /api/v2/public/get_book_summary_by_instrument``
  → ONE body carrying both ``volume_usd`` → ``vol24h_quote`` (USD)
  and ``open_interest`` → ``oi`` (venue units: USD notional for
  perps/futures — the crate-header convention).
- ``okx`` (WS7): ``GET /api/v5/market/ticker?instId=I`` →
  ``volCcy24h`` → ``vol24h_quote`` (venue units: quote ccy on spot,
  venue-defined on derivatives); plus — DERIVATIVE instIds only
  (≥ 3 ``-``-separated segments; the venue errors OI on spot) —
  ``GET /api/v5/public/open-interest?instId=I`` → ``oi``
  (contract units).
- ``hyperliquid`` (WS8): ONE ``POST /info metaAndAssetCtxs`` body
  per cycle covers the whole perp universe → ``dayNtlVlm`` (USD
  notional) → ``vol24h_quote`` for every configured PERP coin
  (``@spot`` / ``#outcome`` coins carry no ctx on this endpoint and
  are skipped).
- ``bybit`` / ``bybit-linear`` (WS9): ONE
  ``GET /v5/market/tickers?category=…&symbol=S`` per target →
  ``turnover24h`` → ``vol24h_quote`` (quote units); linear bodies
  also carry ``openInterest`` → ``oi`` (base/contract units).

Values are stored in RAW VENUE UNITS — the consumer resolves
semantics via ``(venue, descriptor, kind)`` exactly like the candle
``v`` column.

Snapshot semantics: rows are hourly buckets — PK ``(venue,
descriptor, kind, hour_ts)`` with ``INSERT OR REPLACE`` — so re-runs
within an hour refresh in place (idempotent by construction) and an
hourly cadence yields one row per hour. The M3/C6+ window may fold
this module into the hourly agent; running by hand any time is safe.

Best-effort discipline throughout (candles/fetchers pattern):
transport failure / unusable body = counted + skipped, never a
crash; the cycle exits 0 with a per-lane stats report on stderr. No
live API calls in tests — HTTP rides the injectable
``claude_worker.candles.Http`` transport pair.
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

BUDGET_ENV: str = "CLAUDE_WORKER_REFDATA_BUDGET_PER_H"
BUDGET_PER_H_DEFAULT: int = 30

MS_1H: int = 3_600_000

KIND_VOL24H: str = "vol24h_quote"
KIND_OI: str = "oi"

_SCHEMA: str = """
CREATE TABLE IF NOT EXISTS refdata (
  venue      INTEGER NOT NULL,
  descriptor TEXT    NOT NULL,
  kind       TEXT    NOT NULL CHECK (kind IN ('vol24h_quote','oi')),
  hour_ts    INTEGER NOT NULL,
  value      REAL    NOT NULL,
  fetched_ts INTEGER NOT NULL,
  PRIMARY KEY (venue, descriptor, kind, hour_ts)
) WITHOUT ROWID;
"""


class LaneStats(typing.NamedTuple):
    """Per-lane cycle accounting (report surface)."""

    targets: int
    rows: int
    failed: int
    budget_out: bool


# ---- store ---------------------------------------------------------------


def ensure_schema(conn: sqlite3.Connection) -> None:
    """Create the ``refdata`` table beside candles (same store —
    ``claude_worker.candles.open_db`` opens it)."""
    conn.executescript(_SCHEMA)


def upsert_snapshot(
    conn: sqlite3.Connection,
    venue: int,
    descriptor: str,
    kind: str,
    now_ms: int,
    value: float,
) -> None:
    """One hourly-bucketed snapshot row (latest write within the hour
    wins — snapshot semantics, module docs)."""
    hour_ts = (now_ms // MS_1H) * MS_1H
    conn.execute(
        "INSERT OR REPLACE INTO refdata"
        " (venue,descriptor,kind,hour_ts,value,fetched_ts)"
        " VALUES (?,?,?,?,?,?)",
        (venue, descriptor, kind, hour_ts, value, now_ms),
    )


# ---- body parsers (strict; skip-never-crash discipline) ------------------


def _float_field(obj: object, key: str) -> float | None:
    """A quoted-decimal field (``"quoteVolume":"123.45"``) as float.
    ``None`` = absent/malformed (venue numbers arrive as strings)."""
    if not isinstance(obj, dict):
        return None
    raw = typing.cast(dict[str, object], obj).get(key)
    if not isinstance(raw, str):
        return None
    try:
        return float(raw)
    except ValueError:
        return None


def parse_bn_ticker24h(raw: str) -> float | None:
    """Binance spot/USDS-M ``ticker/24hr`` → ``quoteVolume``. The two
    products share the field. ``None`` = unusable body."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    return _float_field(obj, "quoteVolume")


def parse_bn_open_interest(raw: str) -> float | None:
    """``/fapi/v1/openInterest`` → ``openInterest`` (base units)."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    return _float_field(obj, "openInterest")


def parse_okx_ticker24h(raw: str) -> float | None:
    """WS7: OKX ``market/ticker`` → ``data[0].volCcy24h``. ``None`` =
    unusable body (including ``code != "0"``)."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict) or typing.cast(dict[str, object], obj).get("code") != "0":
        return None
    data = typing.cast(dict[str, object], obj).get("data")
    if not isinstance(data, list) or not data:
        return None
    return _float_field(typing.cast(list[object], data)[0], "volCcy24h")


def parse_okx_open_interest(raw: str) -> float | None:
    """WS7: OKX ``public/open-interest`` → ``data[0].oi`` (contract
    units)."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict) or typing.cast(dict[str, object], obj).get("code") != "0":
        return None
    data = typing.cast(dict[str, object], obj).get("data")
    if not isinstance(data, list) or not data:
        return None
    return _float_field(typing.cast(list[object], data)[0], "oi")


def parse_hl_asset_ctxs(raw: str) -> dict[str, float] | None:
    """WS8: HL ``metaAndAssetCtxs`` → ``{coin: dayNtlVlm}`` for the
    whole perp universe (the two top-level arrays are
    ordinal-aligned). Rows missing either half are skipped; ``None``
    = unusable body."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, list) or len(obj) < 2:
        return None
    pair = typing.cast(list[object], obj)
    meta, ctxs = pair[0], pair[1]
    if not isinstance(meta, dict) or not isinstance(ctxs, list):
        return None
    universe = typing.cast(dict[str, object], meta).get("universe")
    if not isinstance(universe, list):
        return None
    out: dict[str, float] = {}
    rows = typing.cast(list[object], universe)
    ctx_rows = typing.cast(list[object], ctxs)
    for i, row in enumerate(rows):
        if i >= len(ctx_rows) or not isinstance(row, dict):
            continue
        name = typing.cast(dict[str, object], row).get("name")
        if not isinstance(name, str) or not name:
            continue
        vol = _float_field(ctx_rows[i], "dayNtlVlm")
        if vol is not None:
            out[name] = vol
    return out


def parse_bybit_ticker(raw: str) -> tuple[float | None, float | None] | None:
    """WS9: Bybit ``market/tickers`` → ``(turnover24h,
    openInterest)`` from ``result.list[0]`` (OI present on linear
    only). ``None`` = unusable body (including ``retCode != 0``)."""
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
    if not isinstance(rows, list) or not rows:
        return None
    row = typing.cast(list[object], rows)[0]
    return _float_field(row, "turnover24h"), _float_field(row, "openInterest")


def parse_deribit_book_summary(raw: str) -> tuple[float | None, float | None] | None:
    """``get_book_summary_by_instrument`` → ``(volume_usd,
    open_interest)`` from ``result[0]``. Either element may be absent
    on exotic instruments (``None`` element); ``None`` return =
    unusable body."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict):
        return None
    result = typing.cast(dict[str, object], obj).get("result")
    if not isinstance(result, list) or not result:
        return None
    row = typing.cast(list[object], result)[0]
    if not isinstance(row, dict):
        return None
    row_d = typing.cast(dict[str, object], row)

    def num(key: str) -> float | None:
        v = row_d.get(key)
        if isinstance(v, bool) or not isinstance(v, (int, float)):
            return None
        return float(v)

    return num("volume_usd"), num("open_interest")


# ---- lane fetchers -------------------------------------------------------


def _fetch_target(
    conn: sqlite3.Connection,
    http: claude_worker.candles.Http,
    lane: claude_worker.candles.Lane,
    target: claude_worker.candles.LaneTarget,
    now_ms: int,
    budget: claude_worker.features.RestBudget,
) -> tuple[int, bool, bool]:
    """Fetch + upsert one target's snapshots. Returns
    ``(rows, failed, budget_out)``."""
    rows = 0
    if lane.name == "binance":
        if not budget.try_acquire():
            return 0, False, True
        raw = http.get(
            f"https://{http.hosts['binance']}/api/v3/ticker/24hr?symbol={target.instrument}"
        )
        vol = parse_bn_ticker24h(raw) if raw is not None else None
        if vol is None:
            return 0, True, False
        upsert_snapshot(conn, target.venue, target.descriptor, KIND_VOL24H, now_ms, vol)
        return 1, False, False
    if lane.name == "binance-usdm":
        if not budget.try_acquire():
            return 0, False, True
        host = http.hosts["binance-usdm"]
        raw = http.get(f"https://{host}/fapi/v1/ticker/24hr?symbol={target.instrument}")
        vol = parse_bn_ticker24h(raw) if raw is not None else None
        failed = False
        if vol is None:
            failed = True
        else:
            upsert_snapshot(conn, target.venue, target.descriptor, KIND_VOL24H, now_ms, vol)
            rows += 1
        if not budget.try_acquire():
            return rows, failed, True
        raw = http.get(f"https://{host}/fapi/v1/openInterest?symbol={target.instrument}")
        oi = parse_bn_open_interest(raw) if raw is not None else None
        if oi is None:
            failed = True
        else:
            upsert_snapshot(conn, target.venue, target.descriptor, KIND_OI, now_ms, oi)
            rows += 1
        return rows, failed, False
    if lane.name == "deribit":
        if not budget.try_acquire():
            return 0, False, True
        raw = http.get(
            f"https://{http.hosts['deribit']}/api/v2/public/get_book_summary_by_instrument"
            f"?instrument_name={target.instrument}"
        )
        parsed = parse_deribit_book_summary(raw) if raw is not None else None
        if parsed is None:
            return 0, True, False
        vol_usd, oi = parsed
        if vol_usd is not None:
            upsert_snapshot(conn, target.venue, target.descriptor, KIND_VOL24H, now_ms, vol_usd)
            rows += 1
        if oi is not None:
            upsert_snapshot(conn, target.venue, target.descriptor, KIND_OI, now_ms, oi)
            rows += 1
        return rows, rows == 0, False
    if lane.name == "okx":
        if not budget.try_acquire():
            return 0, False, True
        raw = http.get(
            f"https://{http.hosts['okx']}/api/v5/market/ticker?instId={target.instrument}"
        )
        vol = parse_okx_ticker24h(raw) if raw is not None else None
        failed = False
        if vol is None:
            failed = True
        else:
            upsert_snapshot(conn, target.venue, target.descriptor, KIND_VOL24H, now_ms, vol)
            rows += 1
        # WS7: OI exists on derivatives only (SWAP/FUTURES/options —
        # ≥ 3 hyphen-separated segments); the venue errors it on spot.
        if target.instrument.count("-") >= 2:
            if not budget.try_acquire():
                return rows, failed, True
            raw = http.get(
                f"https://{http.hosts['okx']}/api/v5/public/open-interest"
                f"?instId={target.instrument}"
            )
            oi = parse_okx_open_interest(raw) if raw is not None else None
            if oi is None:
                failed = True
            else:
                upsert_snapshot(conn, target.venue, target.descriptor, KIND_OI, now_ms, oi)
                rows += 1
        return rows, failed, False
    if lane.name == "bybit" or lane.name == "bybit-linear":
        # WS9: one tickers body carries vol (+ OI on linear).
        if not budget.try_acquire():
            return 0, False, True
        category = "spot" if lane.name == "bybit" else "linear"
        raw = http.get(
            f"https://{http.hosts['bybit']}/v5/market/tickers"
            f"?category={category}&symbol={target.instrument}"
        )
        parsed = parse_bybit_ticker(raw) if raw is not None else None
        if parsed is None:
            return 0, True, False
        vol, oi = parsed
        if vol is not None:
            upsert_snapshot(conn, target.venue, target.descriptor, KIND_VOL24H, now_ms, vol)
            rows += 1
        if oi is not None:
            upsert_snapshot(conn, target.venue, target.descriptor, KIND_OI, now_ms, oi)
            rows += 1
        return rows, rows == 0, False
    raise ValueError(f"refdata: no fetcher for lane {lane.name}")


def _fetch_hl_lane(
    conn: sqlite3.Connection,
    http: claude_worker.candles.Http,
    lane: claude_worker.candles.Lane,
    now_ms: int,
    budget: claude_worker.features.RestBudget,
) -> LaneStats:
    """WS8: the Hyperliquid lane — ONE ``metaAndAssetCtxs`` body
    covers every configured PERP coin (spot ``@`` / outcome ``#``
    coins carry no ctx here and are skipped, the
    ``coin_wants_asset_ctx`` gating law)."""
    perp_targets = [
        t
        for t in lane.targets
        if not t.instrument.startswith("@") and not t.instrument.startswith("#")
    ]
    if not perp_targets:
        return LaneStats(len(lane.targets), 0, 0, False)
    if not budget.try_acquire():
        return LaneStats(len(lane.targets), 0, 0, True)
    body = json.dumps({"type": "metaAndAssetCtxs"}, separators=(",", ":"))
    raw = http.post(f"https://{http.hosts['hyperliquid']}/info", body)
    vols = parse_hl_asset_ctxs(raw) if raw is not None else None
    if vols is None:
        return LaneStats(len(lane.targets), 0, len(perp_targets), False)
    rows = 0
    failed = 0
    for t in perp_targets:
        vol = vols.get(t.instrument)
        if vol is None:
            failed += 1
            continue
        upsert_snapshot(conn, t.venue, t.descriptor, KIND_VOL24H, now_ms, vol)
        rows += 1
    return LaneStats(len(lane.targets), rows, failed, False)


def run_cycle(
    conn: sqlite3.Connection,
    lanes: list[claude_worker.candles.Lane],
    http: claude_worker.candles.Http,
    now_ms: int,
    budget_per_h: int,
    report: collections.abc.Callable[[str], None],
) -> None:
    """One snapshot cycle over every implemented lane × target.
    Budgets are per venue (fetchers convention), shared with nothing —
    this module runs serialized like every worker invocation."""
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
        if lane.name == "hyperliquid":
            # WS8: one body for the whole lane.
            stats = _fetch_hl_lane(conn, http, lane, now_ms, budget)
        else:
            rows = 0
            failed = 0
            budget_out = False
            for target in lane.targets:
                r, f, b = _fetch_target(conn, http, lane, target, now_ms, budget)
                rows += r
                failed += 1 if f else 0
                if b:
                    budget_out = True
                    break
            stats = LaneStats(len(lane.targets), rows, failed, budget_out)
        conn.commit()
        report(
            f"refdata: {lane.name}: targets={stats.targets} rows={stats.rows}"
            f" failed={stats.failed}{' BUDGET' if stats.budget_out else ''}"
        )


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.refdata")
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
    lanes = claude_worker.candles.read_universe_lanes(universe)
    if lanes is None:
        print(f"refdata: unusable universe file {universe}", file=sys.stderr)
        return 1
    if not lanes:
        print(f"refdata: no instruments in {universe}", file=sys.stderr)
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
        total = conn.execute("SELECT count(*) FROM refdata").fetchone()[0]
        stamp = datetime.datetime.fromtimestamp(now_ms / 1000, tz=datetime.timezone.utc)
        print(
            f"refdata: cycle done {stamp.isoformat()} rows={total} db={db_path}",
            file=sys.stderr,
        )
    finally:
        conn.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
