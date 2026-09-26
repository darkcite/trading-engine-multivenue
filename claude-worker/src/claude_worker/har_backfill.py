# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har_backfill -- 1 m history for the HAR H3 sources (W1b, plan §12.4 / §13.5, H3.6).

The long-tenor HAR needs whole UTC days: 30 to be warm, then 60 pairs per
tenor to fit. The candles lane never goes backward (an empty series gets
48 h, then only forward gap-fill), so a series the engine starts on today
would take months to fit. The venues keep their 1 m history back to
listing, and this module fetches it: for every ``har.toml`` series, its
feed and each fallback, into ``candles.db`` under the venue's own
descriptor, ``source = rest``, through :func:`claude_worker.candles.upsert_rest`
-- the store's laws unchanged (a closed ``rest`` bar is immutable; a
disagreeing refetch is logged in ``candle_conflicts``).

A module (``python -m claude_worker.har_backfill``), never a verb. It runs
on the Mac only -- Binance, OKX and Bybit answer the cloud container with
HTTP 451 -- serialised under the worker guard like every other writer of
the store, and it is idempotent: a second run fetches only what the first
did not.

Every source -- the feed AND each fallback -- is kept LIVE: stored from its
listing (or the horizon) to the last CLOSED minute, and gap-filled forward
every hour by ``candles-cycle.sh`` (the 2026-09-26 ruling "keep fallbacks
live": the drift ``har_seed compare`` measures between a feed and its
fallbacks is a live number, never one frozen at the overlap). Only closed
bars are stored: every row this module writes is final.

Per source, two walks. Every request asks for the page just BEFORE a bound
(measured 2026-09-26: Binance spot and USDⓈ-M ``klines`` with only
``endTime``, Bybit ``kline`` with only ``end`` and OKX ``history-candles``
with ``after`` all answer that page; before the listing they answer an
empty one):

- **down**: from the stored first minute (or the last closed minute, for
  an empty series) to the listing or the horizon, whichever comes later.
  Each page ends where the stored series begins.
- **up**: from the stored last minute to the last closed minute. The
  NEWEST page first: when it reaches back to the stored last minute it is
  the whole fill (the hourly case: one page), and when it holds nothing
  newer the source has stopped printing (delisted, dark) and that one page
  is all it costs. A gap of a few pages of bars is then paged backward and
  written once it connects (a stretch the venue printed nothing in costs
  nothing here: the venue skips empty time). A longer gap is walked
  FORWARD a page at a time -- each request's bound one page past the stored
  last minute, so the page starts right after it and is written as it
  lands; a SHORT answer is re-paged below its oldest bar, so a truncated
  page never becomes a hole.

Either way the store only ever grows contiguously from its ends, so an
interrupted walk (a page budget, a transport failure) leaves no hole and
the next run resumes from what it stored. A page budget is spent on the
FEEDS first (every series' feed, then every first fallback, and so on), and
``--max-pages-per-source`` caps what one source may take of it in a run, so
a source that lags far behind (or keeps paging a long stretch its venue
printed nothing in) never starves the rest. A stretch of weeks a venue
printed NOTHING in, followed by fresh bars, is more than a capped run's
forward walk crosses: the hourly report says ``capped`` for that source, and
one unbounded run (no ``--max-pages``) crosses it.

After ``--import-from``, every walkable source's interior HOLES (a stretch
of more than an hour with no stored minute) are bridged by the same up-walk:
an imported block of history need not meet the rows the store already had.

``--import-from DB`` first copies every source's 1 m rows from another
store -- the go-live dry run's -- with ``INSERT OR IGNORE``: a row this
store already holds is never replaced (its closed bars are final).

Offline worker tool: allocation is fine. Convention: full ``import x``
only. No ``from x import y``.
"""

import argparse
import collections.abc
import dataclasses
import datetime
import os
import pathlib
import sqlite3
import sys
import time

import httpx

import claude_worker.candles
import claude_worker.fetchers
import claude_worker.frames
import claude_worker.har_config

#: UTC days kept behind ``now``: ``har_seed``'s replay (240) plus slack.
HORIZON_DAYS_DEFAULT: int = 250
#: Rows per ``INSERT OR IGNORE`` batch of ``--import-from``.
IMPORT_BATCH: int = 10_000
#: Backward pages the up-walk buffers after the newest one before it walks
#: forward instead.
PROBE_PAGES: int = 2
_IMPORT_SELECT: str = (
    "SELECT venue,descriptor,tf,open_ts,o,h,l,c,v,n,source,fetched_ts"
    " FROM candles WHERE descriptor=? AND tf=?"
)
_IMPORT_SELECT_PRE_C5: str = (
    "SELECT venue,descriptor,tf,open_ts,o,h,l,c,v,NULL,source,fetched_ts"
    " FROM candles WHERE descriptor=? AND tf=?"
)
#: Tries per page before the walk stops (resumable next run).
TRIES: int = 4
_MIN_MS: int = claude_worker.candles.MS_1M
_DAY_MS: int = claude_worker.candles.MS_1D
_TF: str = "1m"
#: A stretch with no stored minute longer than this, inside a source's
#: stored span, is a hole the ``--import-from`` run bridges.
BRIDGE_MIN_MS: int = 60 * _MIN_MS


@dataclasses.dataclass(frozen=True, slots=True)
class Walker:
    """How one venue pages a 1 m series backward."""

    #: The candles lane (and ``Http.hosts`` key) the descriptor belongs to.
    lane: str
    venue: int
    #: The venue's request form of the instrument.
    instrument: str
    page_bars: int
    #: Seconds between requests: under the venue's public IP limit.
    pause_s: float


def walker_for(descriptor: str) -> Walker | None:
    """The backward walker for ``descriptor``, or ``None`` when its venue
    has none here (the candles lane or the capture fills it)."""
    prefix, _, inst = descriptor.partition(":")
    frames = claude_worker.frames
    if not inst:
        return None
    if prefix == "binance":
        # /api/v3/klines: weight 2 at any limit, max 1000.
        return Walker("binance", frames.VENUE_BINANCE, inst.upper(), 1000, 0.1)
    if prefix == "binance-usdm":
        # /fapi/v1/klines: limit 1500 weighs 10 of the 2400/min IP budget.
        return Walker("binance-usdm", frames.VENUE_BINANCE, inst.upper(), 1500, 0.3)
    if prefix == "okx":
        # history-candles: 20 requests / 2 s, 100 bars a page.
        return Walker("okx", frames.VENUE_OKX, inst, claude_worker.candles.OKX_PAGE_LIMIT, 0.12)
    if prefix in ("bybit", "bybit-linear"):
        return Walker(prefix, frames.VENUE_BYBIT, inst, claude_worker.candles.BYBIT_PAGE_BARS, 0.12)
    return None


def page_url(http: claude_worker.candles.Http, w: Walker, before_ms: int) -> str:
    """The request for the page of bars opened strictly before ``before_ms``."""
    if w.lane == "binance":
        return (
            f"https://{http.hosts['binance']}/api/v3/klines?symbol={w.instrument}"
            f"&interval=1m&endTime={before_ms - 1}&limit={w.page_bars}"
        )
    if w.lane == "binance-usdm":
        return (
            f"https://{http.hosts['binance-usdm']}/fapi/v1/klines?symbol={w.instrument}"
            f"&interval=1m&endTime={before_ms - 1}&limit={w.page_bars}"
        )
    if w.lane == "okx":
        return (
            f"https://{http.hosts['okx']}/api/v5/market/history-candles?instId={w.instrument}"
            f"&bar=1m&after={before_ms}&limit={w.page_bars}"
        )
    category = "spot" if w.lane == "bybit" else "linear"
    return (
        f"https://{http.hosts['bybit']}/v5/market/kline?category={category}&symbol={w.instrument}"
        f"&interval=1&end={before_ms - 1}&limit={w.page_bars}"
    )


def parse_page(w: Walker, raw: str) -> list[claude_worker.fetchers.Candle] | None:
    """The page's bars oldest first; ``None`` = an unusable body."""
    if w.lane in ("binance", "binance-usdm"):
        parsed = claude_worker.candles.parse_binance_klines(raw)
    elif w.lane == "okx":
        parsed = claude_worker.fetchers.parse_okx_candles(raw)
    else:
        parsed = claude_worker.candles.parse_bybit_kline(raw)
    return None if parsed is None else parsed[0]


@dataclasses.dataclass(slots=True)
class Pager:
    """The transport, the pacing and the run's page budget."""

    http: claude_worker.candles.Http
    sleep: collections.abc.Callable[[float], None] = time.sleep
    #: Pages this run may still fetch; ``None`` = unbounded.
    budget: int | None = None
    pages: int = 0

    def fetch_before(self, w: Walker, before_ms: int) -> list[claude_worker.fetchers.Candle] | None:
        """The bars opened strictly before ``before_ms``, oldest first:
        ``[]`` past the listing, ``None`` when the budget is spent or the
        venue failed :data:`TRIES` times."""
        if self.budget is not None and self.pages >= self.budget:
            return None
        url = page_url(self.http, w, before_ms)
        for k in range(TRIES):
            if k > 0:
                self.sleep(float(2**k))
            raw = self.http.get(url)
            page = None if raw is None else parse_page(w, raw)
            if page is not None:
                self.pages += 1
                self.sleep(w.pause_s)
                return [c for c in page if c.ts_ms < before_ms]
        return None


@dataclasses.dataclass(slots=True)
class SourceReport:
    """What one source's walks did, for the report line."""

    series: str
    descriptor: str
    down_rows: int = 0
    up_rows: int = 0
    conflicts: int = 0
    #: ``listing`` (the venue answered empty), ``horizon``, ``stopped``
    #: (budget or transport), ``capped`` (the source's share of the run
    #: spent) -- both resumable -- or ``-`` (no down walk ran).
    down_end: str = "-"
    #: ``connected`` (reached the last closed minute), ``stopped``,
    #: ``capped`` or ``-``.
    up_end: str = "-"
    first_ts_ms: int | None = None
    last_ts_ms: int | None = None
    #: Why the source was not walked, when it was not.
    skipped: str = ""


def _span(conn: sqlite3.Connection, descriptor: str) -> tuple[int | None, int | None]:
    """The stored first and last minute of ``descriptor`` -- by descriptor
    alone, the way ``har_seed`` reads it (any venue, any source)."""
    row = conn.execute(
        "SELECT min(open_ts), max(open_ts) FROM candles WHERE descriptor=? AND tf=?",
        (descriptor, _TF),
    ).fetchone()
    return (row[0], row[1]) if row is not None else (None, None)


@dataclasses.dataclass(slots=True)
class Run:
    """One backfill run: the store, the pager, ``now`` and the bounds."""

    conn: sqlite3.Connection
    pager: Pager
    now_ms: int
    horizon_days: int = HORIZON_DAYS_DEFAULT
    #: Pages one source may take in this run; ``None`` = unbounded.
    per_source_pages: int | None = None

    @property
    def closed_by(self) -> int:
        """The open of the minute ``now`` is in: every bar before it has closed."""
        return self.now_ms - self.now_ms % _MIN_MS

    @property
    def floor_ms(self) -> int:
        """The horizon: the UTC day ``horizon_days`` before ``now``."""
        return (self.now_ms // _DAY_MS - self.horizon_days) * _DAY_MS


@dataclasses.dataclass(slots=True)
class SourceWalk:
    """The two walks over one source."""

    run: Run
    w: Walker
    rep: SourceReport
    #: The pager's count when this source's walks began (its share).
    start_pages: int = 0

    def _fetch(self, before_ms: int) -> list[claude_worker.fetchers.Candle] | None:
        """A page -- ``None`` once the source's share of the run is spent,
        like the run's budget or the venue failing."""
        cap = self.run.per_source_pages
        if cap is not None and self.run.pager.pages - self.start_pages >= cap:
            return None
        return self.run.pager.fetch_before(self.w, before_ms)

    def _room(self) -> int | None:
        """Pages this source may still take in the run (``None``: unbounded)."""
        left: list[int] = []
        cap = self.run.per_source_pages
        if cap is not None:
            left.append(cap - (self.run.pager.pages - self.start_pages))
        if self.run.pager.budget is not None:
            left.append(self.run.pager.budget - self.run.pager.pages)
        return min(left) if left else None

    @property
    def _halt(self) -> str:
        """Why a walk stopped short: its share, or the run (budget, venue)."""
        cap = self.run.per_source_pages
        if cap is not None and self.run.pager.pages - self.start_pages >= cap:
            return "capped"
        return "stopped"

    def _upsert(self, bars: list[claude_worker.fetchers.Candle]) -> int:
        st = claude_worker.candles.upsert_rest(
            self.run.conn, self.w.venue, self.rep.descriptor, _TF, _MIN_MS, bars, self.run.now_ms
        )
        self.rep.conflicts += st.conflicts
        return st.inserted

    def down(self, before_ms: int) -> None:
        """Page from ``before_ms`` down to the listing or the horizon,
        upserting each page as it lands (it ends where the store begins)."""
        floor_ms = self.run.floor_ms
        while before_ms > floor_ms:
            page = self._fetch(before_ms)
            if page is None:
                self.rep.down_end = self._halt
                return
            if not page:
                self.rep.down_end = "listing"
                return
            keep = [c for c in page if c.ts_ms >= floor_ms]
            if keep:
                self.rep.down_rows += self._upsert(keep)
            if len(keep) < len(page):
                break
            before_ms = page[0].ts_ms
        self.rep.down_end = "horizon"

    def up(self, last_ms: int, until_ms: int) -> None:
        """Fill ``(last_ms, until_ms)`` (module doc): the newest page, a short
        buffered probe backward, else the forward walk."""
        head = self._fetch(until_ms)
        if head is None:
            self.rep.up_end = self._halt
            return
        buffered = [c for c in head if last_ms < c.ts_ms < until_ms]
        # A page holding a bar at or before the stored last minute (or no
        # bar at all) covers everything after it: connected.
        reached = len(buffered) < len(head) or not head
        # Probe only when the run leaves room for the probes AND a forward
        # page after them -- a share too small for both walks forward, so
        # every page it takes is kept.
        room = self._room()
        limit = PROBE_PAGES if room is None or room > PROBE_PAGES else 0
        probes = 0
        while not reached and buffered[0].ts_ms > last_ms + _MIN_MS and probes < limit:
            page = self._fetch(buffered[0].ts_ms)
            if page is None:
                self.rep.up_end = self._halt
                return
            older = [c for c in page if c.ts_ms > last_ms]
            buffered = older + buffered
            reached = len(older) < len(page) or not page
            probes += 1
        if reached or buffered[0].ts_ms <= last_ms + _MIN_MS:
            if buffered:
                self.rep.up_rows += self._upsert(buffered)
            self.rep.up_end = "connected"
            return
        self._forward(last_ms, until_ms)

    def _forward(self, last_ms: int, until_ms: int) -> None:
        """Walk ``(last_ms, until_ms)`` FORWARD, a page of minutes at a time,
        writing each window as it lands (resumable at any page)."""
        span = self.w.page_bars * _MIN_MS
        cursor = last_ms
        while cursor + _MIN_MS < until_ms:
            before_ms = min(cursor + _MIN_MS + span, until_ms)
            bars = self._window(cursor, before_ms)
            if bars is None:
                self.rep.up_end = self._halt
                return
            if bars:
                self.rep.up_rows += self._upsert(bars)
            cursor = before_ms - _MIN_MS
        self.rep.up_end = "connected"

    def _window(
        self, cursor: int, before_ms: int
    ) -> list[claude_worker.fetchers.Candle] | None:
        """Every bar in ``(cursor, before_ms)`` -- at most one page of
        minutes. A SHORT answer that stops above ``cursor`` is re-paged
        below its oldest bar until the venue answers nothing newer than
        ``cursor``: a truncated page never becomes a hole, and a stretch the
        venue printed nothing in is one it has no bars for."""
        out: list[claude_worker.fetchers.Candle] = []
        bound = before_ms
        while True:
            page = self._fetch(bound)
            if page is None:
                return None
            newer = [c for c in page if cursor < c.ts_ms < bound]
            out = newer + out
            if (
                len(newer) < len(page)
                or not newer
                or len(page) >= self.w.page_bars
                or newer[0].ts_ms <= cursor + _MIN_MS
            ):
                return out
            bound = newer[0].ts_ms


def backfill_source(run: Run, series: str, descriptor: str, until_ms: int) -> SourceReport:
    """Both walks for one source (module doc)."""
    rep = SourceReport(series, descriptor)
    w = walker_for(descriptor)
    if w is None:
        rep.skipped = "no backward REST walk for this venue"
    else:
        walk = SourceWalk(run, w, rep, run.pager.pages)
        first, last = _span(run.conn, descriptor)
        if first is None or last is None:
            walk.down(until_ms)
        else:
            if last + _MIN_MS < until_ms:
                walk.up(last, until_ms)
            if first > run.floor_ms:
                walk.down(first)
    rep.first_ts_ms, rep.last_ts_ms = _span(run.conn, descriptor)
    return rep


def backfill_series(run: Run, series: claude_worker.har_config.Series) -> list[SourceReport]:
    """The feed, then each fallback -- every one to the last closed minute
    and back to its listing or the horizon (module doc: kept live)."""
    return [backfill_source(run, series.name, d, run.closed_by) for d in series.sources()]


def interior_holes(
    conn: sqlite3.Connection, descriptor: str, min_ms: int = BRIDGE_MIN_MS
) -> list[tuple[int, int]]:
    """``(last_before, first_after)`` of every stretch longer than ``min_ms``
    with no stored minute inside ``descriptor``'s stored span (by
    descriptor alone, like :func:`_span`), oldest first."""
    holes: list[tuple[int, int]] = []
    prev: int | None = None
    for (ts,) in conn.execute(
        "SELECT open_ts FROM candles WHERE descriptor=? AND tf=? ORDER BY open_ts",
        (descriptor, _TF),
    ):
        if prev is not None and ts - prev > min_ms:
            holes.append((prev, ts))
        prev = ts
    return holes


def bridge_series(run: Run, series: claude_worker.har_config.Series) -> list[SourceReport]:
    """After ``--import-from``: walk every interior hole of every walkable
    source with the up-walk over ``(last_before, first_after)`` -- one page
    when the venue printed nothing there either."""
    out: list[SourceReport] = []
    for d in series.sources():
        w = walker_for(d)
        if w is None:
            continue
        for lo, hi in interior_holes(run.conn, d):
            rep = SourceReport(series.name, d, first_ts_ms=lo, last_ts_ms=hi)
            SourceWalk(run, w, rep, run.pager.pages).up(lo, hi)
            out.append(rep)
    return out


def bridge_line(rep: SourceReport) -> str:
    """One line per bridged hole."""
    return (
        f"har-backfill bridge {rep.series} {rep.descriptor}:"
        f" {_stamp(rep.first_ts_ms)} .. {_stamp(rep.last_ts_ms)}"
        f" +{rep.up_rows} ({rep.up_end}) conflicts={rep.conflicts}"
    )


def import_rows(
    conn: sqlite3.Connection,
    src_path: pathlib.Path,
    series: list[claude_worker.har_config.Series],
    report: collections.abc.Callable[[str], None],
) -> int:
    """Copy every source's 1 m rows from the store at ``src_path`` into
    ``conn``, keeping every row ``conn`` holds (``INSERT OR IGNORE``);
    returns the rows added. A missing file is an error, never a new empty
    store, and the source is read under ``query_only``."""
    if not src_path.is_file():
        raise FileNotFoundError(f"no store to import from at {src_path}")
    src = sqlite3.connect(src_path)
    total = 0
    try:
        src.execute("PRAGMA query_only = 1")
        cols = {row[1] for row in src.execute("PRAGMA table_info(candles)")}
        # A pre-C5 store has no tick-count column: its rows carry NULL.
        select = _IMPORT_SELECT if "n" in cols else _IMPORT_SELECT_PRE_C5
        for s in series:
            for d in s.sources():
                cur = src.execute(select, (d, _TF))
                before = conn.total_changes
                while rows := cur.fetchmany(IMPORT_BATCH):
                    conn.executemany(
                        "INSERT OR IGNORE INTO candles"
                        " (venue,descriptor,tf,open_ts,o,h,l,c,v,n,source,fetched_ts)"
                        " VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                        rows,
                    )
                conn.commit()
                added = conn.total_changes - before
                total += added
                report(f"har-backfill import {s.name} {d}: +{added} row(s) from {src_path}")
    finally:
        src.close()
    return total


def _stamp(ts_ms: int | None) -> str:
    if ts_ms is None:
        return "-"
    return datetime.datetime.fromtimestamp(ts_ms / 1000, datetime.UTC).strftime("%Y-%m-%dT%H:%MZ")


def report_line(rep: SourceReport) -> str:
    """One line per source."""
    head = f"har-backfill {rep.series} {rep.descriptor}:"
    stored = f"stored {_stamp(rep.first_ts_ms)} .. {_stamp(rep.last_ts_ms)}"
    if rep.skipped:
        return f"{head} skipped ({rep.skipped}); {stored}"
    return (
        f"{head} down +{rep.down_rows} ({rep.down_end}) up +{rep.up_rows} ({rep.up_end})"
        f" conflicts={rep.conflicts}; {stored}"
    )


def run_all(
    run: Run,
    series: list[claude_worker.har_config.Series],
    report: collections.abc.Callable[[str], None],
    bridge: bool = False,
) -> bool:
    """Every source, the FEEDS first -- the rows the seeds' newest days are
    made of never wait behind a fallback catching up on a budget -- then
    each rank of fallbacks, series order within a rank; after
    ``--import-from`` (``bridge``) every interior hole before any walk.
    ``False`` when a walk stopped short."""
    complete = True
    short = ("stopped", "capped")
    for s in series if bridge else []:
        for rep in bridge_series(run, s):
            report(bridge_line(rep))
            complete = complete and rep.up_end not in short
    depth = max((len(s.sources()) for s in series), default=0)
    for rank in range(depth):
        for s in series:
            sources = s.sources()
            if rank >= len(sources):
                continue
            rep = backfill_source(run, s.name, sources[rank], run.closed_by)
            report(report_line(rep))
            complete = complete and rep.down_end not in short and rep.up_end not in short
    tail = "" if complete else " -- INCOMPLETE, rerun resumes"
    report(f"har-backfill: {run.pager.pages} page(s){tail}")
    return complete


def _say(line: str) -> None:
    """A report line, flushed: the hourly lane's log shows progress live."""
    print(line, flush=True)


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only -- never a worker verb)."""
    ap = argparse.ArgumentParser(prog="claude_worker.har_backfill")
    ap.add_argument("--har-toml", type=pathlib.Path, default=None)
    ap.add_argument("--db", default=None)
    ap.add_argument("--series", action="append", default=None, help="only these names")
    ap.add_argument("--horizon-days", type=int, default=HORIZON_DAYS_DEFAULT)
    ap.add_argument("--max-pages", type=int, default=None, help="page budget for this run")
    ap.add_argument(
        "--max-pages-per-source",
        type=int,
        default=None,
        help="pages one source may take of this run",
    )
    ap.add_argument(
        "--import-from",
        type=pathlib.Path,
        default=None,
        help="first copy the sources' 1 m rows from this store (INSERT OR IGNORE)",
    )
    ap.add_argument("--now-ms", type=int, default=None)
    args = ap.parse_args(argv)
    toml_path = (
        args.har_toml.expanduser() if args.har_toml else claude_worker.har_config.default_path()
    )
    try:
        series = claude_worker.har_config.read(toml_path)
    except (OSError, claude_worker.har_config.HarConfigError) as e:
        print(f"har-backfill: {toml_path}: {e}", file=sys.stderr)
        return 2
    if args.series:
        wanted = set(args.series)
        series = [s for s in series if s.name in wanted]
    db = pathlib.Path(
        args.db
        or os.environ.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH
    ).expanduser()
    now = int(time.time() * 1000) if args.now_ms is None else args.now_ms
    conn = claude_worker.candles.open_db(db)
    try:
        if args.import_from is not None:
            try:
                import_rows(conn, args.import_from.expanduser(), series, _say)
            except (OSError, sqlite3.Error) as e:
                print(f"har-backfill: import: {e}", file=sys.stderr)
                return 2
        with httpx.Client() as client:
            http = claude_worker.candles.make_http(client, os.environ)
            pager = Pager(http, sleep=time.sleep, budget=args.max_pages)
            run = Run(conn, pager, now, args.horizon_days, args.max_pages_per_source)
            complete = run_all(run, series, _say, bridge=args.import_from is not None)
    finally:
        conn.close()
    return 0 if complete else 1


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
