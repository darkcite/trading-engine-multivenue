# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""hypercall_history — the Hypercall research-store pullers (HC6).

A standalone MODULE (``python -m claude_worker.hypercall_history``) —
never a worker verb (the frozen CLI surface; the funding/candles
precedent). Data-only (ruling O-HC1): public REST reads, no key, no
account (O-HC9). One cycle runs three lanes, each best-effort under
ONE ``RestBudget``, into tables BESIDE candles in ``candles.db``:

- ``hc_trades`` — ``GET /trades?after_trade_id=<cursor>&limit=1000``:
  the venue's EXCLUSIVE global cursor, ascending. The rows carry the
  maker AND taker wallets, which the WS never sends. The cursor is the
  table's own ``max(trade_id)``, so a cycle resumes exactly where the
  last one stopped and a lost cycle loses nothing.
- ``hc_payouts`` — ``GET /settlement-payouts?wallet=<w>&limit=100&
  offset=<o>`` for each configured wallet (``--wallets`` /
  ``CLAUDE_WORKER_HC_WALLETS``; none configured = the lane is skipped).
  Pages stop at the first page holding nothing new. ``settlement_price``
  is the INTRINSIC per contract, so every in-the-money row pins its
  expiry's settlement: S = K + intrinsic (call), K - intrinsic (put) —
  :func:`settle_prices` derives the (underlying, expiry) -> S table the
  HC7 replicator is judged against and reports rows that disagree.
- ``hc_summary`` — one ``GET /options-summary?currency=<U>&
  include_rfq_provider_quotes=true`` per underlying of ``[hypercall]
  underlyings`` in ``universe.toml``: mark, IV (a FRACTION), underlying
  price, open interest, best bid/ask and the provider count, per
  instrument, stamped with the cycle's ``now``.

Operational law: SERIALIZED like every worker invocation —
``pgrep -f claude-worker`` first (the lane script's job); the store is
WAL with a busy timeout, so an accidental overlap degrades to a wait.
Transport failure / unusable body = counted + skipped, never a crash;
the injectable ``claude_worker.candles.Http`` means no live call in a
test.

Convention: full ``import x`` only.
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

import claude_worker.candles
import claude_worker.features
import claude_worker.fetchers
import claude_worker.frames

HOST_ENV: str = "HYPERCALL_REST_HOST"
HOST_DEFAULT: str = "api.hypercall.xyz"
WALLETS_ENV: str = "CLAUDE_WORKER_HC_WALLETS"
BUDGET_ENV: str = "CLAUDE_WORKER_HC_BUDGET_PER_H"
BUDGET_PER_H_DEFAULT: int = 120
#: The venue's page caps (OpenAPI 3.0.3, 2026-09-25).
TRADES_PAGE: int = 1000
PAYOUTS_PAGE: int = 100
#: Per-cycle page caps, so one cycle stays bounded however far behind.
TRADES_PAGES_MAX: int = 20
PAYOUT_PAGES_MAX: int = 10

_SCHEMA: str = """
CREATE TABLE IF NOT EXISTS hc_trades (
  trade_id   INTEGER PRIMARY KEY,
  ts_ms      INTEGER NOT NULL,
  symbol     TEXT    NOT NULL,
  price      REAL    NOT NULL,
  size       REAL    NOT NULL,
  taker_side TEXT    NOT NULL,
  maker      TEXT    NOT NULL,
  taker      TEXT    NOT NULL,
  maker_fee  REAL    NOT NULL,
  taker_fee  REAL    NOT NULL,
  fetched_ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hc_payouts (
  id               INTEGER PRIMARY KEY,
  wallet           TEXT    NOT NULL,
  symbol           TEXT    NOT NULL,
  expiry_s         INTEGER NOT NULL,
  position_size    REAL    NOT NULL,
  settlement_price REAL    NOT NULL,
  settlement_value REAL    NOT NULL,
  entry_price      REAL,
  net_pnl          REAL,
  created_ms       INTEGER,
  fetched_ts       INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hc_summary (
  instrument    TEXT    NOT NULL,
  ts_ms         INTEGER NOT NULL,
  venue         INTEGER NOT NULL,
  mark_price    REAL,
  mark_iv       REAL,
  underlying_px REAL,
  open_interest REAL,
  bid           REAL,
  ask           REAL,
  providers     INTEGER NOT NULL,
  PRIMARY KEY (instrument, ts_ms)
) WITHOUT ROWID;
"""


#: ``<UND>-<YYYYMMDD>-<STRIKE>-<C|P>``: four dash-separated fields, an
#: eight-digit date.
_NAME_FIELDS: int = 4
_DATE_DIGITS: int = 8


class Ctx(typing.NamedTuple):
    """One cycle's shared state: the store, the transport, the REST host,
    the per-hour call budget and the cycle's wall stamp."""

    conn: sqlite3.Connection
    http: claude_worker.candles.Http
    host: str
    budget: claude_worker.features.RestBudget
    now_ms: int


class CycleStats(typing.NamedTuple):
    """What one cycle did, per lane (rows inserted / calls failed)."""

    trades: int
    payouts: int
    summary: int
    failed: int
    skipped: int


class OptionName(typing.NamedTuple):
    """``<UND>-<YYYYMMDD>-<STRIKE>-<C|P>`` (decimal strikes exist)."""

    underlying: str
    strike: float
    call: bool


def ensure_schema(conn: sqlite3.Connection) -> None:
    """Create the three tables beside candles."""
    conn.executescript(_SCHEMA)


def parse_option_name(name: str) -> OptionName | None:
    """An instrument name -> its parts, ``None`` for anything else."""
    parts = name.split("-")
    if len(parts) != _NAME_FIELDS or parts[3] not in ("C", "P") or len(parts[1]) != _DATE_DIGITS:
        return None
    if not parts[0] or not parts[1].isdigit():
        return None
    try:
        strike = float(parts[2])
    except ValueError:
        return None
    if strike <= 0.0:
        return None
    return OptionName(parts[0], strike, parts[3] == "C")


def _num(v: typing.Any) -> float | None:
    """A JSON number or numeric string -> float; ``None`` otherwise."""
    if isinstance(v, bool) or v is None:
        return None
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, str):
        try:
            return float(v)
        except ValueError:
            return None
    return None


def _data_rows(raw: str | None) -> list[dict[str, typing.Any]] | None:
    """``{"success": true, "data": [...]}`` -> the rows; ``None`` when
    the body is not that shape."""
    if raw is None:
        return None
    try:
        body = json.loads(raw)
    except json.JSONDecodeError:
        return None
    if not isinstance(body, dict) or body.get("success") is not True:
        return None
    data = body.get("data")
    if not isinstance(data, list):
        return None
    return [r for r in data if isinstance(r, dict)]


def pull_trades(ctx: Ctx) -> tuple[int, int]:
    """Page ``/trades`` forward from the stored cursor. Returns
    ``(rows inserted, failed calls)``."""
    inserted = failed = 0
    for _ in range(TRADES_PAGES_MAX):
        cursor = ctx.conn.execute("SELECT max(trade_id) FROM hc_trades").fetchone()[0] or 0
        if not ctx.budget.try_acquire():
            break
        rows = _data_rows(
            ctx.http.get(f"https://{ctx.host}/trades?after_trade_id={cursor}&limit={TRADES_PAGE}")
        )
        if rows is None:
            failed += 1
            break
        batch = []
        for r in rows:
            tid, ts = r.get("trade_id"), r.get("timestamp")
            px, size = _num(r.get("price")), _num(r.get("size"))
            mf, tf = _num(r.get("maker_fee")), _num(r.get("taker_fee"))
            fields = (
                r.get("symbol"),
                r.get("taker_side"),
                r.get("maker_address"),
                r.get("taker_address"),
            )
            if not isinstance(tid, int) or not isinstance(ts, int) or None in (px, size, mf, tf):
                continue
            if not all(isinstance(f, str) for f in fields):
                continue
            batch.append(
                (tid, ts, fields[0], px, size, fields[1], fields[2], fields[3], mf, tf, ctx.now_ms)
            )
        with ctx.conn:
            before = ctx.conn.total_changes
            ctx.conn.executemany(
                "INSERT OR IGNORE INTO hc_trades VALUES (?,?,?,?,?,?,?,?,?,?,?)", batch
            )
            inserted += ctx.conn.total_changes - before
        if len(rows) < TRADES_PAGE:
            break
    return inserted, failed


def pull_payouts(ctx: Ctx, wallets: collections.abc.Sequence[str]) -> tuple[int, int]:
    """Page each wallet's settlement payouts until a page holds nothing
    new. Returns ``(rows inserted, failed calls)``."""
    inserted = failed = 0
    for wallet in wallets:
        for page in range(PAYOUT_PAGES_MAX):
            if not ctx.budget.try_acquire():
                return inserted, failed
            rows = _data_rows(
                ctx.http.get(
                    f"https://{ctx.host}/settlement-payouts?wallet={wallet}"
                    f"&limit={PAYOUTS_PAGE}&offset={page * PAYOUTS_PAGE}"
                )
            )
            if rows is None:
                failed += 1
                break
            batch = []
            for r in rows:
                rid, exp = r.get("id"), r.get("expiry_ts")
                size = _num(r.get("position_size"))
                sp, sv = _num(r.get("settlement_price")), _num(r.get("settlement_value"))
                sym, w = r.get("symbol"), r.get("wallet")
                if not isinstance(rid, int) or not isinstance(exp, int) or None in (size, sp, sv):
                    continue
                if not isinstance(sym, str) or not isinstance(w, str):
                    continue
                created = r.get("created_at")
                batch.append(
                    (
                        rid,
                        w,
                        sym,
                        exp,
                        size,
                        sp,
                        sv,
                        _num(r.get("settlement_entry_price")),
                        _num(r.get("net_pnl")),
                        created if isinstance(created, int) else None,
                        ctx.now_ms,
                    )
                )
            with ctx.conn:
                before = ctx.conn.total_changes
                ctx.conn.executemany(
                    "INSERT OR IGNORE INTO hc_payouts VALUES (?,?,?,?,?,?,?,?,?,?,?)", batch
                )
                new = ctx.conn.total_changes - before
            inserted += new
            if new == 0 or len(rows) < PAYOUTS_PAGE:
                break
    return inserted, failed


def snapshot_summary(ctx: Ctx, underlyings: collections.abc.Sequence[str]) -> tuple[int, int]:
    """One Deribit-shaped ``/options-summary`` per underlying. Returns
    ``(rows inserted, failed calls)``."""
    inserted = failed = 0
    for und in underlyings:
        if not ctx.budget.try_acquire():
            break
        raw = ctx.http.get(
            f"https://{ctx.host}/options-summary?currency={und}&include_rfq_provider_quotes=true"
        )
        try:
            body = json.loads(raw) if raw is not None else None
        except json.JSONDecodeError:
            body = None
        result = body.get("result") if isinstance(body, dict) else None
        if not isinstance(result, list):
            failed += 1
            continue
        batch = []
        for r in result:
            if not isinstance(r, dict) or not isinstance(r.get("instrument_name"), str):
                continue
            quotes = r.get("rfq_provider_quotes")
            batch.append(
                (
                    r["instrument_name"],
                    ctx.now_ms,
                    claude_worker.frames.VENUE_HYPERCALL,
                    _num(r.get("mark_price")),
                    _num(r.get("mark_iv")),
                    _num(r.get("underlying_price")),
                    _num(r.get("open_interest")),
                    _num(r.get("bid_price")),
                    _num(r.get("ask_price")),
                    len(quotes) if isinstance(quotes, list) else 0,
                )
            )
        with ctx.conn:
            before = ctx.conn.total_changes
            ctx.conn.executemany(
                "INSERT OR IGNORE INTO hc_summary VALUES (?,?,?,?,?,?,?,?,?,?)", batch
            )
            inserted += ctx.conn.total_changes - before
    return inserted, failed


def settle_prices(
    conn: sqlite3.Connection, tol: float = 1e-6
) -> list[tuple[str, int, float, int, bool]]:
    """``hc_payouts`` -> ``(underlying, expiry_s, S, itm_rows, consistent)``
    per expiry with at least one in-the-money row: S = K + intrinsic for
    a call, K - intrinsic for a put. ``consistent`` is False when two
    rows of one expiry pin S further apart than ``tol x S`` — a table
    that must stay empty before the HC7 gate trusts it."""
    pins: dict[tuple[str, int], list[float]] = {}
    for sym, exp, intrinsic in conn.execute(
        "SELECT symbol, expiry_s, settlement_price FROM hc_payouts WHERE settlement_price > 0"
    ):
        name = parse_option_name(sym)
        if name is None:
            continue
        s = name.strike + intrinsic if name.call else name.strike - intrinsic
        pins.setdefault((name.underlying, exp), []).append(s)
    out: list[tuple[str, int, float, int, bool]] = []
    for (und, exp), ss in sorted(pins.items()):
        mid = sorted(ss)[len(ss) // 2]
        ok = all(abs(s - mid) <= tol * abs(mid) for s in ss)
        out.append((und, exp, mid, len(ss), ok))
    return out


def run_cycle(
    ctx: Ctx,
    underlyings: collections.abc.Sequence[str],
    wallets: collections.abc.Sequence[str],
    report: collections.abc.Callable[[str], None],
) -> CycleStats:
    """One cycle: trades, then payouts, then the summary snapshot, all
    under ``ctx.budget``."""
    trades, f1 = pull_trades(ctx)
    payouts, f2 = (0, 0)
    if wallets:
        payouts, f2 = pull_payouts(ctx, wallets)
    else:
        report("hypercall_history: no wallets configured - payouts lane skipped")
    summary, f3 = snapshot_summary(ctx, underlyings)
    stats = CycleStats(trades, payouts, summary, f1 + f2 + f3, ctx.budget.skipped_total)
    report(
        f"hypercall_history: trades +{trades} payouts +{payouts} summary +{summary}"
        f" failed={stats.failed} budget_skipped={stats.skipped}"
    )
    return stats


def read_underlyings(universe_path: pathlib.Path) -> list[str] | None:
    """``[hypercall] underlyings`` of the universe file (``[]`` when the
    section is absent; ``None`` when the file is unusable)."""
    try:
        doc = tomllib.loads(universe_path.read_text(encoding="utf-8"))
    except OSError, tomllib.TOMLDecodeError:
        return None
    section = doc.get("hypercall", {})
    unds = section.get("underlyings", []) if isinstance(section, dict) else []
    if not isinstance(unds, list) or not all(isinstance(u, str) for u in unds):
        return None
    return list(unds)


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.hypercall_history")
    parser.add_argument("--universe", default=None)
    parser.add_argument("--db", default=None)
    parser.add_argument("--wallets", default=None, help="comma-separated public wallets")
    parser.add_argument("--budget-per-h", type=int, default=None)
    parser.add_argument(
        "--settle-prices", action="store_true", help="print the derived S table and exit"
    )
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    args = parser.parse_args(argv)
    env = os.environ
    db_path = pathlib.Path(
        args.db
        or env.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH
    ).expanduser()
    conn = claude_worker.candles.open_db(db_path)
    try:
        conn.execute("PRAGMA busy_timeout = 30000")
        ensure_schema(conn)
        if args.settle_prices:
            for und, exp, s, n, ok in settle_prices(conn):
                day = datetime.datetime.fromtimestamp(exp, tz=datetime.timezone.utc)
                print(f"{und}\t{day.isoformat()}\t{s:.6f}\t{n}\t{'ok' if ok else 'INCONSISTENT'}")
            return 0
        universe = pathlib.Path(
            args.universe
            or env.get(claude_worker.fetchers.UNIVERSE_FILE_ENV, "")
            or claude_worker.candles.DEFAULT_UNIVERSE_PATH
        ).expanduser()
        underlyings = read_underlyings(universe)
        if underlyings is None:
            print(f"hypercall_history: unusable universe file {universe}", file=sys.stderr)
            return 1
        wallets = [
            w.strip() for w in (args.wallets or env.get(WALLETS_ENV, "")).split(",") if w.strip()
        ]
        budget = args.budget_per_h or int(env.get(BUDGET_ENV, "") or BUDGET_PER_H_DEFAULT)
        now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
        host = env.get(HOST_ENV, "") or HOST_DEFAULT
        with httpx.Client() as client:
            ctx = Ctx(
                conn,
                claude_worker.candles.make_http(client, env),
                host,
                claude_worker.features.RestBudget(budget, 3_600_000_000_000),
                now_ms,
            )
            run_cycle(ctx, underlyings, wallets, lambda line: print(line, file=sys.stderr))
    finally:
        conn.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
