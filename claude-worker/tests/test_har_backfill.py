# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har_backfill -- 1 m history back to listing for the HAR H3 sources (H3.1, H3.6).

A fake venue answers the three backward page shapes measured live on
2026-09-26 (Binance ``endTime``, OKX ``after``, Bybit ``end``); no network.
Pinned: an empty series walks down to its listing, an interrupted walk
leaves no hole and the next run resumes, the up walk is FORWARD -- every
page connects as it lands, so a page budget smaller than the gap still
makes progress --, every fallback is kept LIVE to the last closed minute
(H3.6, the ruling "keep fallbacks live"), ``--import-from`` copies the
sources' rows without replacing any the store holds, and only closed bars
are ever stored.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import sqlite3
import urllib.parse

import pytest

import claude_worker.candles
import claude_worker.har_backfill
import claude_worker.har_config

_MIN: int = 60_000
_DAY: int = 86_400_000
#: 2026-01-01 00:00Z.
_T0: int = 1_767_225_600_000


class FakeVenue:
    """Answers backward pages over one minute history per instrument:
    ``hist[instrument] = [(open_ts, close), ...]`` ascending. ``truncate``
    caps every answer at that many bars -- a venue answering SHORT."""

    def __init__(
        self,
        hist: dict[str, list[tuple[int, float]]],
        fail_after: int | None = None,
        truncate: int | None = None,
    ):
        self.hist = hist
        self.calls: list[str] = []
        self.fail_after = fail_after
        self.truncate = truncate

    def _cut(self, bars: list[tuple[int, float]]) -> list[tuple[int, float]]:
        return bars if self.truncate is None else bars[-self.truncate :]

    def get(self, url: str) -> str | None:
        self.calls.append(url)
        if self.fail_after is not None and len(self.calls) > self.fail_after:
            return None
        u = urllib.parse.urlparse(url)
        q = dict(urllib.parse.parse_qsl(u.query))
        if u.path in ("/fapi/v1/klines", "/api/v3/klines"):
            bars = self._cut(
                [b for b in self.hist[q["symbol"]] if b[0] <= int(q["endTime"])][-int(q["limit"]) :]
            )
            return json.dumps(
                [[t, str(c), str(c), str(c), str(c), "1.0", t + _MIN - 1] for t, c in bars]
            )
        if u.path == "/api/v5/market/history-candles":
            bars = self._cut(
                [b for b in self.hist[q["instId"]] if b[0] < int(q["after"])][-int(q["limit"]) :]
            )
            rows = [
                [str(t), str(c), str(c), str(c), str(c), "1", "1", "1", "1"]
                for t, c in reversed(bars)
            ]
            return json.dumps({"code": "0", "msg": "", "data": rows})
        if u.path == "/v5/market/kline":
            bars = self._cut(
                [b for b in self.hist[q["symbol"]] if b[0] <= int(q["end"])][-int(q["limit"]) :]
            )
            rows = [[str(t), str(c), str(c), str(c), str(c), "1", "1"] for t, c in reversed(bars)]
            return json.dumps({"retCode": 0, "retMsg": "OK", "result": {"list": rows}})
        raise AssertionError(f"unexpected url {url}")


def _http(venue: FakeVenue) -> claude_worker.candles.Http:
    hosts = {k: k for k in ("binance", "binance-usdm", "okx", "bybit")}
    return claude_worker.candles.Http(get=venue.get, post=lambda url, body: None, hosts=hosts)


def _pager(venue: FakeVenue, budget: int | None = None) -> claude_worker.har_backfill.Pager:
    return claude_worker.har_backfill.Pager(_http(venue), sleep=lambda s: None, budget=budget)


def _hist(first: int, last: int, seed: int = 1) -> list[tuple[int, float]]:
    """Minutes ``[first, last]`` inclusive, a deterministic walk."""
    out, px = [], 100.0 + seed
    for t in range(first, last + _MIN, _MIN):
        px = round(px + ((t // _MIN * seed) % 7 - 3) * 0.01, 2)
        out.append((t, px))
    return out


def _stored(conn: sqlite3.Connection, descriptor: str) -> list[tuple[int, float]]:
    return conn.execute(
        "SELECT open_ts, c FROM candles WHERE descriptor=? AND tf='1m' ORDER BY open_ts",
        (descriptor,),
    ).fetchall()


def _bf(
    conn: sqlite3.Connection,
    pager: claude_worker.har_backfill.Pager,
    series: claude_worker.har_config.Series,
    now: int,
    **bounds: int,
) -> list[claude_worker.har_backfill.SourceReport]:
    run = claude_worker.har_backfill.Run(conn, pager, now, **bounds)
    return claude_worker.har_backfill.backfill_series(run, series)


def _series(feed: str, *fallback: str, name: str = "SP500") -> claude_worker.har_config.Series:
    return claude_worker.har_config.Series(name, feed, tuple(fallback))


def test_an_empty_series_walks_down_to_its_listing(tmp_path: pathlib.Path) -> None:
    listing = _T0 + 5 * _DAY + 7 * _MIN
    now = _T0 + 9 * _DAY + 30_000  # mid-minute: the 00:00 bar is still open
    hist = _hist(listing, now - now % _MIN)  # the venue also has the OPEN bar
    venue = FakeVenue({"SPYUSDT": hist})
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    reps = _bf(conn, _pager(venue), _series("binance-usdm:spyusdt"), now)
    assert len(reps) == 1 and reps[0].down_end == "listing" and reps[0].up_end == "-"
    closed = hist[:-1]
    assert _stored(conn, "binance-usdm:spyusdt") == closed, "every closed bar, the open one never"
    assert reps[0].down_rows == len(closed)
    assert (reps[0].first_ts_ms, reps[0].last_ts_ms) == (listing, closed[-1][0])
    # Every row is final: fetched after it closed, so the store's laws make it immutable.
    row = conn.execute("SELECT venue, source, fetched_ts FROM candles LIMIT 1").fetchone()
    assert row == (1, "rest", now)
    # The second run has nothing to fetch but one page proving the listing.
    venue.calls.clear()
    again = _bf(conn, _pager(venue), _series("binance-usdm:spyusdt"), now)
    assert (again[0].down_rows, again[0].up_rows, len(venue.calls)) == (0, 0, 1)


def test_an_interrupted_walk_leaves_no_hole_and_the_next_run_resumes(
    tmp_path: pathlib.Path,
) -> None:
    listing = _T0
    now = _T0 + 3 * _DAY
    hist = _hist(listing, now)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    cut = FakeVenue({"MU-USDT-SWAP": hist}, fail_after=5)
    reps = _bf(conn, _pager(cut), _series("okx:MU-USDT-SWAP", name="MU"), now)
    assert reps[0].down_end == "stopped"
    part = _stored(conn, "okx:MU-USDT-SWAP")
    assert part == hist[-1 - len(part) : -1], "a contiguous run ending at the last closed minute"
    assert len(part) == 5 * 100
    reps = _bf(
        conn, _pager(FakeVenue({"MU-USDT-SWAP": hist})), _series("okx:MU-USDT-SWAP", name="MU"), now
    )
    assert reps[0].down_end == "listing"
    assert _stored(conn, "okx:MU-USDT-SWAP") == hist[:-1]


def _ends(venue: FakeVenue, key: str) -> list[int]:
    return [
        int(dict(urllib.parse.parse_qsl(urllib.parse.urlparse(u).query))[key]) for u in venue.calls
    ]


def test_the_up_walk_takes_the_newest_page_first(tmp_path: pathlib.Path) -> None:
    """The hourly case is ONE page: the newest reaches back to the stored
    last minute. A gap of a few pages is paged backward and written once it
    connects -- a budget that stops it writes nothing, the next run
    completes it."""
    hist = _hist(_T0, _T0 + 2 * _DAY)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    first_now = _T0 + _DAY
    bot = _series("bybit-linear:BOTUSDT", name="BOT")
    _bf(conn, _pager(FakeVenue({"BOTUSDT": hist})), bot, first_now)
    assert _stored(conn, "bybit-linear:BOTUSDT")[-1][0] == first_now - _MIN
    hour = FakeVenue({"BOTUSDT": hist})
    reps = _bf(conn, _pager(hour), bot, first_now + 3_600_000)
    assert (reps[0].up_end, reps[0].up_rows) == ("connected", 60)
    assert _ends(hour, "end")[0] == first_now + 3_600_000 - 1, "the newest page first"
    assert len([u for u in hour.calls]) == 2, "one up page, one proving the listing"
    later = _T0 + 2 * _DAY
    # 1380 missing minutes, Bybit pages 1000: the newest page and one probe.
    reps = _bf(conn, _pager(FakeVenue({"BOTUSDT": hist}), budget=1), bot, later)
    assert (reps[0].up_end, reps[0].up_rows) == ("stopped", 0), "a probe cut short writes nothing"
    reps = _bf(conn, _pager(FakeVenue({"BOTUSDT": hist})), bot, later)
    assert (reps[0].up_end, reps[0].up_rows) == ("connected", 1380)
    assert _stored(conn, "bybit-linear:BOTUSDT") == [b for b in hist if b[0] < later]


def test_a_source_that_stopped_printing_costs_one_page_and_starves_nothing(
    tmp_path: pathlib.Path,
) -> None:
    """The review's case: a fallback delisted 30 days ago sits ahead of the
    BTC feed. Its newest page holds nothing newer than the store, so it
    costs one page an hour -- never the whole budget -- and the feed behind
    it still gap-fills."""
    now = _T0 + 40 * _DAY
    dead = _hist(_T0, _T0 + 10 * _DAY - _MIN, seed=3)
    btc = _hist(_T0, now, seed=2)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    series = _series("okx:SPCX-USDT-SWAP", "binance-usdm:btcusdt", name="X")
    venue = FakeVenue({"SPCX-USDT-SWAP": dead, "BTCUSDT": btc})
    _bf(conn, _pager(venue), series, now - 3_600_000)
    venue.calls.clear()
    reps = _bf(conn, _pager(venue, budget=4), series, now)
    assert (reps[0].up_end, reps[0].up_rows) == ("connected", 0), "dark: one page, nothing new"
    assert (reps[1].up_end, reps[1].up_rows) == ("connected", 60)
    assert sum("history-candles" in u for u in venue.calls) == 2, "one up page + the listing's"
    assert _stored(conn, "binance-usdm:btcusdt")[-1][0] == now - _MIN


def test_the_feeds_go_first_so_a_lagging_fallback_never_holds_them_back(
    tmp_path: pathlib.Path,
) -> None:
    """A budget spent series by series would let MU's fallback, months
    behind, take the hour before ETH's feed; spent feeds first, every feed
    gap-fills and the fallback gets what is left."""
    now = _T0 + 30 * _DAY
    okx = _hist(_T0, now, seed=3)
    mu = _hist(_T0, now, seed=2)
    eth = _hist(_T0, now, seed=5)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    series = [
        _series("binance-usdm:muusdt", "okx:MU-USDT-SWAP", name="MU"),
        _series("binance-usdm:ethusdt", name="ETH"),
    ]
    hist = {"MUUSDT": mu, "MU-USDT-SWAP": okx, "ETHUSDT": eth}
    early = claude_worker.har_backfill.Run(conn, _pager(FakeVenue(hist)), _T0 + _DAY)
    claude_worker.har_backfill.run_all(early, series, lambda line: None)
    feeds = claude_worker.har_backfill.Run(conn, _pager(FakeVenue(hist)), now - 3_600_000)
    for s in series:
        claude_worker.har_backfill.backfill_source(feeds, s.name, s.feed, feeds.closed_by)
    lines: list[str] = []
    run = claude_worker.har_backfill.Run(conn, _pager(FakeVenue(hist), budget=6), now)
    assert not claude_worker.har_backfill.run_all(run, series, lines.append)
    assert [line.split(":")[0] for line in lines[:3]] == [
        "har-backfill MU binance-usdm",
        "har-backfill ETH binance-usdm",
        "har-backfill MU okx",
    ]
    assert "up +60 (connected)" in lines[0] and "up +60 (connected)" in lines[1]
    assert "(stopped)" in lines[2], "the fallback took what was left"


def test_a_long_gap_walks_forward_and_a_share_caps_it(tmp_path: pathlib.Path) -> None:
    """More bars than the newest page and its probes reach: the walk goes
    FORWARD from the stored last minute, writing each page as it lands; a
    per-source share stops it (``capped``) with what it wrote contiguous,
    and the next source still gets its pages."""
    now = _T0 + 10 * _DAY
    okx = _hist(_T0, now, seed=3)
    btc = _hist(_T0, now, seed=2)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    series = _series("okx:MU-USDT-SWAP", "binance-usdm:btcusdt", name="MU")
    _bf(conn, _pager(FakeVenue({"MU-USDT-SWAP": okx, "BTCUSDT": btc})), series, _T0 + _DAY)
    venue = FakeVenue({"MU-USDT-SWAP": okx, "BTCUSDT": btc})
    run = claude_worker.har_backfill.Run(conn, _pager(venue), now, per_source_pages=20)
    reps = claude_worker.har_backfill.backfill_series(run, series)
    # 20 pages: the newest, two probes (discarded), then 17 forward pages.
    assert (reps[0].up_end, reps[0].up_rows) == ("capped", 17 * 100)
    assert _stored(conn, "okx:MU-USDT-SWAP") == [b for b in okx if b[0] < _T0 + _DAY + 1700 * _MIN]
    assert reps[1].up_end == "connected", "the share left the feed its pages"
    assert _stored(conn, "binance-usdm:btcusdt")[-1][0] == now - _MIN
    # A share too small for the probes walks forward at once: it still gains.
    run = claude_worker.har_backfill.Run(conn, _pager(venue, budget=2), now)
    reps = claude_worker.har_backfill.backfill_series(run, series)
    assert (reps[0].up_end, reps[0].up_rows) == ("stopped", 100)
    # Unbounded, the next run finishes the fallback.
    reps = _bf(conn, _pager(FakeVenue({"MU-USDT-SWAP": okx, "BTCUSDT": btc})), series, now)
    assert reps[0].up_end == "connected"
    assert _stored(conn, "okx:MU-USDT-SWAP") == okx[:-1]


def test_a_short_answer_is_repaged_never_a_hole(tmp_path: pathlib.Path) -> None:
    """A venue that answers SHORT (fewer bars than the page's minutes, not
    reaching back to the cursor) is re-paged below the answer's oldest bar:
    the forward walk never jumps a stretch the venue does have bars for."""
    now = _T0 + 3 * _DAY
    okx = _hist(_T0, now, seed=3)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    mu = _series("okx:MU-USDT-SWAP", name="MU")
    _bf(conn, _pager(FakeVenue({"MU-USDT-SWAP": okx})), mu, _T0 + _DAY)
    reps = _bf(conn, _pager(FakeVenue({"MU-USDT-SWAP": okx}, truncate=37)), mu, now)
    assert reps[0].up_end == "connected"
    assert _stored(conn, "okx:MU-USDT-SWAP") == okx[:-1], "every minute, no hole"


def test_a_minute_the_venue_never_had_stays_absent_and_the_walk_goes_on(
    tmp_path: pathlib.Path,
) -> None:
    """A venue gap inside the fill is no hole of the walk's making: the
    pages reach back past it, and nothing refetches it."""
    full = _hist(_T0, _T0 + 3 * _DAY)
    halt = (_T0 + _DAY + 100 * _MIN, _T0 + _DAY + 400 * _MIN)
    hist = [b for b in full if not halt[0] <= b[0] < halt[1]]
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    mu = _series("okx:MU-USDT-SWAP", name="MU")
    _bf(conn, _pager(FakeVenue({"MU-USDT-SWAP": hist})), mu, _T0 + _DAY)
    reps = _bf(conn, _pager(FakeVenue({"MU-USDT-SWAP": hist})), mu, _T0 + 3 * _DAY)
    assert reps[0].up_end == "connected"
    assert _stored(conn, "okx:MU-USDT-SWAP") == [b for b in hist if b[0] < _T0 + 3 * _DAY]


def test_every_fallback_is_kept_live_to_the_last_closed_minute(tmp_path: pathlib.Path) -> None:
    """H3.6: the feed and each fallback, listing to the last closed minute --
    the drift ``compare`` measures is a live number (the ruling "keep
    fallbacks live"), and the next hour gap-fills all three."""
    now = _T0 + 20 * _DAY
    feed = _hist(_T0 + 10 * _DAY, now + _DAY, seed=2)
    okx = _hist(_T0 + 4 * _DAY, now + _DAY, seed=3)
    bybit = _hist(_T0, now + _DAY, seed=4)
    venue = FakeVenue({"NVDAUSDT": feed, "NVDA-USDT-SWAP": okx, "BOT": bybit})
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    series = _series("binance-usdm:nvdausdt", "okx:NVDA-USDT-SWAP", "bybit-linear:BOT", name="NVDA")
    reps = _bf(conn, _pager(venue), series, now)
    assert [r.down_end for r in reps] == ["listing", "listing", "listing"]
    for descriptor, hist in (
        ("binance-usdm:nvdausdt", feed),
        ("okx:NVDA-USDT-SWAP", okx),
        ("bybit-linear:BOT", bybit),
    ):
        assert _stored(conn, descriptor) == [b for b in hist if b[0] < now], descriptor
    lines = [claude_worker.har_backfill.report_line(r) for r in reps]
    assert lines[1].startswith(
        "har-backfill NVDA okx:NVDA-USDT-SWAP: down +23040 (listing) up +0 (-)"
    )
    assert lines[1].endswith("stored 2026-01-05T00:00Z .. 2026-01-20T23:59Z")
    # An hour later: every source gap-fills forward, one page each.
    later = now + 3_600_000
    reps = _bf(conn, _pager(venue), series, later)
    assert [(r.up_end, r.up_rows) for r in reps] == [("connected", 60)] * 3
    assert [r.last_ts_ms for r in reps] == [later - _MIN] * 3


def test_a_fallback_behind_a_source_covering_the_horizon_is_fetched_too(
    tmp_path: pathlib.Path,
) -> None:
    """H3.6: nothing is skipped for being "not needed" -- BTC spot stays
    live behind the USDⓈ-M feed, each back to the same horizon."""
    now = _T0 + 12 * _DAY
    venue = FakeVenue({"BTCUSDT": _hist(_T0, now)})
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    series = _series("binance-usdm:btcusdt", "binance:btcusdt", name="BTC")
    reps = _bf(conn, _pager(venue), series, now, horizon_days=5)
    assert [r.down_end for r in reps] == ["horizon", "horizon"]
    assert [r.skipped for r in reps] == ["", ""]
    for d in ("binance-usdm:btcusdt", "binance:btcusdt"):
        assert _stored(conn, d)[0][0] == _T0 + 7 * _DAY and _stored(conn, d)[-1][0] == now - _MIN
    assert any("/api/v3/" in c for c in venue.calls)


def test_a_source_without_a_walker_is_skipped_and_its_fallback_still_walked(
    tmp_path: pathlib.Path,
) -> None:
    """``hypercall-idx:`` is capture-only: never walked; the fallback behind
    it is walked to the last closed minute like any other."""
    now = _T0 + 10 * _DAY
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    idx = _hist(_T0 + 8 * _DAY, now - _MIN, seed=5)
    conn.executemany(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,c,source,fetched_ts)"
        " VALUES (9,?,'1m',?,?,'capture',?)",
        [("hypercall-idx:SP500", t, c, now) for t, c in idx],
    )
    conn.commit()
    okx = _hist(_T0, now, seed=6)
    venue = FakeVenue({"SPY-USDT-SWAP": okx})
    reps = _bf(conn, _pager(venue), _series("hypercall-idx:SP500", "okx:SPY-USDT-SWAP"), now)
    assert reps[0].skipped == "no backward REST walk for this venue"
    assert reps[0].first_ts_ms == _T0 + 8 * _DAY
    assert _stored(conn, "okx:SPY-USDT-SWAP") == okx[:-1]


def _mk_store(path: pathlib.Path, rows: dict[str, list[tuple[int, float]]], fetched: int) -> None:
    conn = claude_worker.candles.open_db(path)
    for d, hist in rows.items():
        conn.executemany(
            "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts)"
            " VALUES (1,?,'1m',?,?,?,?,?,1.0,'rest',?)",
            [(d, t, c, c, c, c, fetched) for t, c in hist],
        )
    conn.commit()
    conn.close()


def test_import_copies_every_source_and_never_replaces_a_stored_row(
    tmp_path: pathlib.Path,
) -> None:
    """``--import-from``: the go-live dry run's rows for every ``har.toml``
    source, ``INSERT OR IGNORE`` -- the store's closed bars are final."""
    src = tmp_path / "dryrun.db"
    feed = _hist(_T0, _T0 + _DAY, seed=2)
    okx = _hist(_T0, _T0 + _DAY, seed=3)
    other = _hist(_T0, _T0 + _MIN, seed=4)
    _mk_store(src, {"binance-usdm:muusdt": feed, "okx:MU-USDT-SWAP": okx, "x:else": other}, 7)
    conn = claude_worker.candles.open_db(tmp_path / "live.db")
    mine = [(t, c + 1.0) for t, c in feed[:10]]
    conn.executemany(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,c,source,fetched_ts)"
        " VALUES (1,'binance-usdm:muusdt','1m',?,?,'rest',9)",
        mine,
    )
    conn.commit()
    lines: list[str] = []
    series = [_series("binance-usdm:muusdt", "okx:MU-USDT-SWAP", name="MU")]
    added = claude_worker.har_backfill.import_rows(conn, src, series, lines.append)
    assert added == (len(feed) - 10) + len(okx)
    assert _stored(conn, "binance-usdm:muusdt") == mine + feed[10:], "the store's rows kept"
    assert _stored(conn, "okx:MU-USDT-SWAP") == okx
    assert _stored(conn, "x:else") == [], "only the har.toml sources"
    head = f"har-backfill import MU binance-usdm:muusdt: +{len(feed) - 10} row(s) from {src}"
    assert lines[0] == head
    assert claude_worker.har_backfill.import_rows(conn, src, series, lines.append) == 0
    with pytest.raises(FileNotFoundError):
        claude_worker.har_backfill.import_rows(conn, tmp_path / "absent.db", series, lines.append)
    assert not (tmp_path / "absent.db").exists(), "a missing store is never created"


def test_the_page_requests_are_the_measured_shapes() -> None:
    http = _http(FakeVenue({}))
    urls = {
        d: claude_worker.har_backfill.page_url(
            http, claude_worker.har_backfill.walker_for(d), 1_000_000
        )
        for d in (
            "binance:btcusdt",
            "binance-usdm:spyusdt",
            "okx:MU-USDT-SWAP",
            "bybit:BTCUSDT",
            "bybit-linear:BOTUSDT",
        )
    }
    assert (
        urls["binance:btcusdt"]
        == "https://binance/api/v3/klines?symbol=BTCUSDT&interval=1m&endTime=999999&limit=1000"
    )
    assert urls["binance-usdm:spyusdt"] == (
        "https://binance-usdm/fapi/v1/klines?symbol=SPYUSDT&interval=1m&endTime=999999&limit=1500"
    )
    assert urls["okx:MU-USDT-SWAP"] == (
        "https://okx/api/v5/market/history-candles?instId=MU-USDT-SWAP&bar=1m&after=1000000&limit=100"
    )
    assert (
        urls["bybit:BTCUSDT"]
        == "https://bybit/v5/market/kline?category=spot&symbol=BTCUSDT&interval=1&end=999999&limit=1000"
    )
    assert urls["bybit-linear:BOTUSDT"].startswith(
        "https://bybit/v5/market/kline?category=linear&symbol=BOTUSDT"
    )
    for d in (
        "hypercall-idx:SP500",
        "hyperliquid:xyz:SP500",
        "mexc-perp:SPY_USDT",
        "deribit:BTC-PERPETUAL",
        "okx",
    ):
        assert claude_worker.har_backfill.walker_for(d) is None


def test_a_failing_venue_is_retried_with_backoff_then_the_walk_stops(
    tmp_path: pathlib.Path,
) -> None:
    slept: list[float] = []
    venue = FakeVenue({"SPYUSDT": _hist(_T0, _T0 + _DAY)}, fail_after=0)
    pager = claude_worker.har_backfill.Pager(_http(venue), sleep=slept.append)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    reps = _bf(conn, pager, _series("binance-usdm:spyusdt"), _T0 + _DAY)
    assert reps[0].down_end == "stopped" and reps[0].down_rows == 0
    assert (len(venue.calls), slept) == (claude_worker.har_backfill.TRIES, [2.0, 4.0, 8.0])


def test_an_import_that_does_not_meet_the_store_is_bridged(tmp_path: pathlib.Path) -> None:
    """The go-live hazard the review found: the candles lane fetched a newly
    appended perp's last 48 h before the dry run's rows were imported, and
    the two blocks do not meet. The ``--import-from`` run bridges every
    interior hole with the up-walk, so no empty day is left behind."""
    now = _T0 + 8 * _DAY
    feed = _hist(_T0, now, seed=2)
    src = tmp_path / "dryrun.db"
    _mk_store(src, {"binance-usdm:muusdt": [b for b in feed if b[0] < _T0 + 2 * _DAY]}, 7)
    conn = claude_worker.candles.open_db(tmp_path / "live.db")
    lane = [b for b in feed if _T0 + 5 * _DAY <= b[0] < _T0 + 7 * _DAY]
    conn.executemany(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,c,source,fetched_ts)"
        " VALUES (1,'binance-usdm:muusdt','1m',?,?,'rest',9)",
        lane,
    )
    conn.commit()
    series = [_series("binance-usdm:muusdt", name="MU")]
    claude_worker.har_backfill.import_rows(conn, src, series, lambda line: None)
    holes = claude_worker.har_backfill.interior_holes(conn, "binance-usdm:muusdt")
    assert holes == [(_T0 + 2 * _DAY - _MIN, _T0 + 5 * _DAY)]
    lines: list[str] = []
    run = claude_worker.har_backfill.Run(conn, _pager(FakeVenue({"MUUSDT": feed})), now)
    assert claude_worker.har_backfill.run_all(run, series, lines.append, bridge=True)
    assert lines[0] == (
        "har-backfill bridge MU binance-usdm:muusdt: 2026-01-02T23:59Z .. 2026-01-06T00:00Z"
        " +4320 (connected) conflicts=0"
    )
    assert _stored(conn, "binance-usdm:muusdt") == feed[:-1], "one contiguous series"
    assert claude_worker.har_backfill.interior_holes(conn, "binance-usdm:muusdt") == []
    # Without --import-from nothing scans for holes: the hourly run stays cheap.
    lines.clear()
    run = claude_worker.har_backfill.Run(conn, _pager(FakeVenue({"MUUSDT": feed})), now)
    claude_worker.har_backfill.run_all(run, series, lines.append)
    assert not any(line.startswith("har-backfill bridge") for line in lines)


def test_main_reads_har_toml_and_reports(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    toml = tmp_path / "har.toml"
    toml.write_text(
        '[[series]]\nname = "MU"\nfeed = "binance-usdm:muusdt"\n'
        '[[series]]\nname = "BTC"\nfeed = "hypercall-idx:BTC"\n',
        encoding="utf-8",
    )
    now = _T0 + 2 * _DAY
    venue = FakeVenue({"MUUSDT": _hist(_T0 + _DAY, now)})
    monkeypatch.setattr(claude_worker.candles, "make_http", lambda client, env: _http(venue))
    monkeypatch.setattr(claude_worker.har_backfill.time, "sleep", lambda s: None)
    db = tmp_path / "c.db"
    argv = ["--har-toml", str(toml), "--db", str(db), "--now-ms", str(now), "--series", "MU"]
    assert claude_worker.har_backfill.main(argv) == 0
    out = capsys.readouterr().out.splitlines()
    assert out[0].startswith("har-backfill MU binance-usdm:muusdt: down +1440 (listing)")
    assert out[-1] == "har-backfill: 2 page(s)"
    # A page budget that stops a walk makes the run INCOMPLETE (exit 1), resumable.
    db2 = tmp_path / "c2.db"
    argv2 = ["--har-toml", str(toml), "--db", str(db2), "--now-ms", str(now), "--max-pages", "0"]
    assert claude_worker.har_backfill.main(argv2) == 1
    assert "INCOMPLETE" in capsys.readouterr().out
    bad = tmp_path / "bad.toml"
    bad.write_text("[[series]]\n", encoding="utf-8")
    assert claude_worker.har_backfill.main(["--har-toml", str(bad), "--db", str(db)]) == 2
    # --import-from runs first; a missing store refuses the run (2).
    src = tmp_path / "dryrun.db"
    _mk_store(src, {"binance-usdm:muusdt": _hist(_T0, _T0 + _DAY)}, 7)
    db3 = tmp_path / "c3.db"
    argv3 = ["--har-toml", str(toml), "--db", str(db3), "--now-ms", str(now), "--series", "MU"]
    assert claude_worker.har_backfill.main([*argv3, "--import-from", str(src)]) == 0
    out = capsys.readouterr().out.splitlines()
    assert out[0] == f"har-backfill import MU binance-usdm:muusdt: +1441 row(s) from {src}"
    assert out[1].startswith("har-backfill MU binance-usdm:muusdt: down +0 (listing) up +1439")
    missing = tmp_path / "missing.db"
    assert claude_worker.har_backfill.main([*argv3, "--import-from", str(missing)]) == 2
