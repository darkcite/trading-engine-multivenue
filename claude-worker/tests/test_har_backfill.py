# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har_backfill -- 1 m history back to listing for the HAR H3 sources (H3.1).

A fake venue answers the three backward page shapes measured live on
2026-09-26 (Binance ``endTime``, OKX ``after``, Bybit ``end``); no network.
Pinned: an empty series walks down to its listing, an interrupted walk
leaves no hole and the next run resumes, the up walk connects or writes
nothing, each fallback stops at its newer source's first minute plus the
overlap, a fallback behind a source that covers the horizon is not
fetched, and only closed bars are ever stored.

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
    ``hist[instrument] = [(open_ts, close), ...]`` ascending."""

    def __init__(self, hist: dict[str, list[tuple[int, float]]], fail_after: int | None = None):
        self.hist = hist
        self.calls: list[str] = []
        self.fail_after = fail_after

    def get(self, url: str) -> str | None:
        self.calls.append(url)
        if self.fail_after is not None and len(self.calls) > self.fail_after:
            return None
        u = urllib.parse.urlparse(url)
        q = dict(urllib.parse.parse_qsl(u.query))
        if u.path in ("/fapi/v1/klines", "/api/v3/klines"):
            bars = [b for b in self.hist[q["symbol"]] if b[0] <= int(q["endTime"])][
                -int(q["limit"]) :
            ]
            return json.dumps(
                [[t, str(c), str(c), str(c), str(c), "1.0", t + _MIN - 1] for t, c in bars]
            )
        if u.path == "/api/v5/market/history-candles":
            bars = [b for b in self.hist[q["instId"]] if b[0] < int(q["after"])][-int(q["limit"]) :]
            rows = [
                [str(t), str(c), str(c), str(c), str(c), "1", "1", "1", "1"]
                for t, c in reversed(bars)
            ]
            return json.dumps({"code": "0", "msg": "", "data": rows})
        if u.path == "/v5/market/kline":
            bars = [b for b in self.hist[q["symbol"]] if b[0] <= int(q["end"])][-int(q["limit"]) :]
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


def test_the_up_walk_connects_or_writes_nothing(tmp_path: pathlib.Path) -> None:
    hist = _hist(_T0, _T0 + 2 * _DAY)
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    first_now = _T0 + _DAY
    _bf(
        conn,
        _pager(FakeVenue({"BOTUSDT": hist})),
        _series("bybit-linear:BOTUSDT", name="BOT"),
        first_now,
    )
    assert _stored(conn, "bybit-linear:BOTUSDT")[-1][0] == first_now - _MIN
    later = _T0 + 2 * _DAY
    # A budget that runs out before the walk reaches the store: nothing is written.
    starved = FakeVenue({"BOTUSDT": hist})
    reps = _bf(conn, _pager(starved, budget=1), _series("bybit-linear:BOTUSDT", name="BOT"), later)
    assert reps[0].up_end == "stopped" and reps[0].up_rows == 0
    assert _stored(conn, "bybit-linear:BOTUSDT")[-1][0] == first_now - _MIN
    reps = _bf(
        conn,
        _pager(FakeVenue({"BOTUSDT": hist})),
        _series("bybit-linear:BOTUSDT", name="BOT"),
        later,
    )
    assert reps[0].up_end == "connected" and reps[0].up_rows == 1440
    assert _stored(conn, "bybit-linear:BOTUSDT") == [b for b in hist if b[0] < later]


def test_each_fallback_stops_at_its_newer_source_plus_the_overlap(tmp_path: pathlib.Path) -> None:
    now = _T0 + 20 * _DAY
    feed = _hist(_T0 + 10 * _DAY, now, seed=2)
    okx = _hist(_T0 + 4 * _DAY, now, seed=3)
    bybit = _hist(_T0, now, seed=4)
    venue = FakeVenue({"NVDAUSDT": feed, "NVDA-USDT-SWAP": okx, "BOT": bybit})
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    series = _series("binance-usdm:nvdausdt", "okx:NVDA-USDT-SWAP", "bybit-linear:BOT", name="NVDA")
    reps = _bf(conn, _pager(venue), series, now, overlap_days=2)
    assert [r.down_end for r in reps] == ["listing", "listing", "listing"]
    assert _stored(conn, "binance-usdm:nvdausdt") == feed[:-1]
    assert _stored(conn, "okx:NVDA-USDT-SWAP") == [b for b in okx if b[0] < _T0 + 12 * _DAY]
    assert _stored(conn, "bybit-linear:BOT") == [b for b in bybit if b[0] < _T0 + 6 * _DAY]
    lines = [claude_worker.har_backfill.report_line(r) for r in reps]
    assert lines[1].startswith(
        "har-backfill NVDA okx:NVDA-USDT-SWAP: down +11520 (listing) up +0 (-)"
    )
    assert lines[1].endswith("stored 2026-01-05T00:00Z .. 2026-01-12T23:59Z")


def test_a_fallback_behind_a_source_covering_the_horizon_is_not_fetched(
    tmp_path: pathlib.Path,
) -> None:
    now = _T0 + 12 * _DAY
    venue = FakeVenue({"BTCUSDT": _hist(_T0, now)})
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    series = _series("binance-usdm:btcusdt", "binance:btcusdt", name="BTC")
    reps = _bf(conn, _pager(venue), series, now, horizon_days=5)
    assert reps[0].down_end == "horizon"
    assert _stored(conn, "binance-usdm:btcusdt")[0][0] == _T0 + 7 * _DAY
    assert reps[1].skipped == "not needed: binance-usdm:btcusdt covers the horizon"
    assert not any("/api/v3/" in c for c in venue.calls)


def test_a_source_without_a_walker_still_bounds_the_next(tmp_path: pathlib.Path) -> None:
    """``hypercall-idx:`` is capture-only: never walked, but its stored
    first minute is where the next fallback stops."""
    now = _T0 + 10 * _DAY
    conn = claude_worker.candles.open_db(tmp_path / "c.db")
    idx = _hist(_T0 + 8 * _DAY, now - _MIN, seed=5)
    conn.executemany(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,c,source,fetched_ts)"
        " VALUES (9,?,'1m',?,?,'capture',?)",
        [("hypercall-idx:SP500", t, c, now) for t, c in idx],
    )
    conn.commit()
    venue = FakeVenue({"SPY-USDT-SWAP": _hist(_T0, now, seed=6)})
    series = _series("hypercall-idx:SP500", "okx:SPY-USDT-SWAP")
    reps = _bf(conn, _pager(venue), series, now, overlap_days=1)
    assert reps[0].skipped == "no backward REST walk for this venue"
    assert reps[0].first_ts_ms == _T0 + 8 * _DAY
    assert _stored(conn, "okx:SPY-USDT-SWAP")[-1][0] == _T0 + 9 * _DAY - _MIN


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
