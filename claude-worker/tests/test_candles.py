"""candles.db tests (M3 C4) — additive; frozen 202 + 7-verb surface
untouched. No live API calls: transports are injected fakes; venue
payloads mirror the real wire shapes the pinned fetchers parsers
expect (Binance klines fixtures mirror the venue docs)."""

import json
import pathlib
import sqlite3

import claude_worker.candles
import claude_worker.features
import claude_worker.fetchers
import claude_worker.frames


MS_1M = claude_worker.candles.MS_1M
MS_1H = claude_worker.candles.MS_1H
MS_1D = claude_worker.candles.MS_1D
NOW = 1_787_400_000_000  # 2026-08-22-ish wall ms


def mk_candle(ts_ms: int, base: float = 100.0) -> claude_worker.fetchers.Candle:
    return claude_worker.fetchers.Candle(
        ts_ms=ts_ms, open=base, high=base + 2, low=base - 1, close=base + 1, volume=5.0
    )


def db(tmp_path: pathlib.Path) -> sqlite3.Connection:
    return claude_worker.candles.open_db(tmp_path / "candles.db")


def bn_target() -> claude_worker.candles.LaneTarget:
    return claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_BINANCE, "binance:btcusdt", "BTCUSDT"
    )


def bn_lane(target: claude_worker.candles.LaneTarget) -> claude_worker.candles.Lane:
    return claude_worker.candles.Lane(
        "binance", claude_worker.frames.VENUE_BINANCE, [target], backward=False
    )


def klines_json(candles: list[claude_worker.fetchers.Candle]) -> str:
    rows = [
        [
            candle.ts_ms,
            str(candle.open),
            str(candle.high),
            str(candle.low),
            str(candle.close),
            str(candle.volume),
            candle.ts_ms + MS_1M - 1,
            "0",
            0,
            "0",
            "0",
            "0",
        ]
        for candle in candles
    ]
    return json.dumps(rows)


def okx_json(candles: list[claude_worker.fetchers.Candle]) -> str:
    # OKX wire: NEWEST-first rows of strings.
    rows = [
        [
            str(candle.ts_ms),
            str(candle.open),
            str(candle.high),
            str(candle.low),
            str(candle.close),
            str(candle.volume),
            "0",
            "0",
            "1",
        ]
        for candle in sorted(candles, key=lambda x: -x.ts_ms)
    ]
    return json.dumps({"code": "0", "msg": "", "data": rows})


def http_none() -> claude_worker.candles.Http:
    return claude_worker.candles.Http(
        get=lambda url: None,
        post=lambda url, body: None,
        hosts={
            "binance": "bn.test",
            "binance-usdm": "bnf.test",
            "okx": "okx.test",
            "deribit": "dbt.test",
            "hyperliquid": "hl.test",
        },
    )


def budget(n: int) -> claude_worker.features.RestBudget:
    return claude_worker.features.RestBudget(n, claude_worker.fetchers.BUDGET_WINDOW_NS)


# ---- store laws ----------------------------------------------------------


def test_upsert_inserts_and_open_bar_updates(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = bn_target()
    open_bar_ts = NOW - MS_1M // 2  # still open
    bars = [mk_candle(NOW - 2 * MS_1M), mk_candle(open_bar_ts, base=200.0)]
    st = claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, bars, NOW
    )
    assert st.inserted == 2 and st.conflicts == 0
    # The row was stored MID-LIFE (open at fetch): the post-close
    # refetch FINALIZES it — legal §9.4 open-bar upsert, no conflict.
    closed_now = open_bar_ts + MS_1M + 1
    st2 = claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, [mk_candle(open_bar_ts, base=201.0)], closed_now
    )
    assert st2.updated_open == 1 and st2.conflicts == 0
    # Now the stored row was CLOSED at fetch — disagreement is a
    # conflict, value immutable.
    st3 = claude_worker.candles.upsert_rest(
        conn,
        t.venue,
        t.descriptor,
        "1m",
        MS_1M,
        [mk_candle(open_bar_ts, base=202.0)],
        closed_now + 1_000,
    )
    assert st3.conflicts == 1 and st3.updated_open == 0
    row = conn.execute(
        "SELECT o FROM candles WHERE descriptor=? AND tf='1m' AND open_ts=?",
        (t.descriptor, open_bar_ts),
    ).fetchone()
    assert row[0] == 201.0


def test_closed_bar_conflict_keeps_original_and_logs(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = bn_target()
    ts = NOW - 10 * MS_1M
    claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, [mk_candle(ts, base=100.0)], NOW
    )
    st = claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, [mk_candle(ts, base=999.0)], NOW
    )
    assert st.conflicts == 1
    kept = conn.execute(
        "SELECT o FROM candles WHERE open_ts=?", (ts,)
    ).fetchone()
    assert kept[0] == 100.0, "closed rest bar is immutable"
    logged = conn.execute(
        "SELECT o, first_seen_ts FROM candle_conflicts WHERE open_ts=?", (ts,)
    ).fetchone()
    assert logged[0] == 999.0
    first_seen = logged[1]
    # Re-conflict later: first_seen_ts survives (COALESCE law).
    st2 = claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, [mk_candle(ts, base=998.0)], NOW + 5_000
    )
    assert st2.conflicts == 1
    again = conn.execute(
        "SELECT o, first_seen_ts FROM candle_conflicts WHERE open_ts=?", (ts,)
    ).fetchone()
    assert again[0] == 998.0 and again[1] == first_seen


def test_unchanged_and_capture_supersede(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = bn_target()
    ts = NOW - 5 * MS_1M
    bar = mk_candle(ts)
    claude_worker.candles.upsert_rest(conn, t.venue, t.descriptor, "1m", MS_1M, [bar], NOW)
    st = claude_worker.candles.upsert_rest(conn, t.venue, t.descriptor, "1m", MS_1M, [bar], NOW)
    assert st.unchanged == 1 and st.conflicts == 0
    # A capture row on another PK is replaced by rest (source hierarchy).
    ts2 = NOW - 6 * MS_1M
    conn.execute(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts)"
        " VALUES (?,?,?,?, 1,2,0.5,1.5, NULL,'capture',1)",
        (t.venue, t.descriptor, "1m", ts2),
    )
    st2 = claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, [mk_candle(ts2, base=50.0)], NOW
    )
    assert st2.superseded_capture == 1
    src = conn.execute("SELECT source, o FROM candles WHERE open_ts=?", (ts2,)).fetchone()
    assert src == ("rest", 50.0)


def test_max_open_ts(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = bn_target()
    assert claude_worker.candles.max_open_ts(conn, t.venue, t.descriptor, "1m") is None
    claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M,
        [mk_candle(NOW - 3 * MS_1M), mk_candle(NOW - MS_1M)], NOW,
    )
    assert claude_worker.candles.max_open_ts(conn, t.venue, t.descriptor, "1m") == NOW - MS_1M


# ---- Binance klines parser ----------------------------------------------


def test_parse_binance_klines_good_and_sorted() -> None:
    a, b = mk_candle(NOW - 2 * MS_1M), mk_candle(NOW - MS_1M, base=101.0)
    parsed = claude_worker.candles.parse_binance_klines(klines_json([b, a]))
    assert parsed is not None
    candles, malformed = parsed
    assert malformed == 0
    assert [x.ts_ms for x in candles] == [a.ts_ms, b.ts_ms]
    assert candles[1].close == 102.0


def test_parse_binance_klines_malformed_rows_counted() -> None:
    good = json.loads(klines_json([mk_candle(NOW - MS_1M)]))
    rows = [
        good[0],
        [],  # short
        ["ts", "1", "2", "3", "4", "5"],  # non-int ts
        [True, "1", "2", "3", "4", "5"],  # bool ts
        [NOW, 1.0, "2", "3", "4", "5"],  # non-str price
        [NOW, "x", "2", "3", "4", "5"],  # unparsable price
    ]
    parsed = claude_worker.candles.parse_binance_klines(json.dumps(rows))
    assert parsed is not None
    candles, malformed = parsed
    assert len(candles) == 1 and malformed == 5


def test_parse_binance_klines_unusable_bodies() -> None:
    assert claude_worker.candles.parse_binance_klines("not json") is None
    assert claude_worker.candles.parse_binance_klines('{"a":1}') is None


# ---- universe lanes ------------------------------------------------------


def test_read_universe_lanes_m1_shape(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "universe.toml"
    p.write_text(
        """
[polymarket]
markets = ["111:222"]

[binance]
spot = ["btcusdt", "ethusdt"]
usdm = ["btcusdt"]

[okx]
instruments = ["BTC-USDT", "ETH-USDT-SWAP"]
depth = false

[deribit]
instruments = ["BTC-PERPETUAL"]

[hyperliquid]
coins = ["BTC", "ETH"]

[pairs]
map = ["0:0"]
""",
        encoding="utf-8",
    )
    lanes = claude_worker.candles.read_universe_lanes(p)
    assert lanes is not None
    by_name = {lane.name: lane for lane in lanes}
    assert set(by_name) == {"binance", "binance-usdm", "okx", "deribit", "hyperliquid"}
    assert [t.descriptor for t in by_name["binance"].targets] == [
        "binance:btcusdt",
        "binance:ethusdt",
    ]
    assert by_name["binance"].targets[0].instrument == "BTCUSDT"
    assert [t.descriptor for t in by_name["binance-usdm"].targets] == ["binance-usdm:btcusdt"]
    assert by_name["okx"].backward and not by_name["binance"].backward
    assert [t.descriptor for t in by_name["hyperliquid"].targets] == [
        "hyperliquid:BTC",
        "hyperliquid:ETH",
    ]
    # PM deliberately absent (§9.7 capture lane).
    assert all("polymarket" not in lane.name for lane in lanes)


def test_read_universe_lanes_missing_or_empty(tmp_path: pathlib.Path) -> None:
    assert claude_worker.candles.read_universe_lanes(tmp_path / "absent.toml") is None
    p = tmp_path / "empty.toml"
    p.write_text("[polymarket]\nmarkets = []\n", encoding="utf-8")
    assert claude_worker.candles.read_universe_lanes(p) == []


# ---- backfill bounds -----------------------------------------------------


def test_backfill_bounds() -> None:
    f = claude_worker.candles.backfill_start_ms
    assert f("1m", NOW, "binance", {}) == NOW - 48 * MS_1H
    assert f("1m", NOW, "binance", {"CLAUDE_WORKER_CANDLES_BACKFILL_1M_H": "24"}) == NOW - 24 * MS_1H
    assert f("1h", NOW, "binance", {}) == NOW - 90 * MS_1D
    assert f("1d", NOW, "binance", {}) == 0, "listing lifetime"
    assert f("1d", NOW, "okx", {}) == NOW - 400 * MS_1D, "OKX cheapness carve-out"
    assert f("1d", NOW, "deribit", {}) == claude_worker.candles.DERIBIT_1D_FLOOR_MS
    assert f("1d", NOW, "hyperliquid", {}) == claude_worker.candles.HL_1D_FLOOR_MS


# ---- forward gap-fill ----------------------------------------------------


class ForwardVenue:
    """Fake Binance: serves klines pages from a fixed bar list."""

    def __init__(self, bars: list[claude_worker.fetchers.Candle], page: int = 3) -> None:
        self.bars = bars
        self.page = page
        self.calls: list[int] = []

    def get(self, url: str) -> str | None:
        start = int(url.split("startTime=")[1].split("&")[0])
        self.calls.append(start)
        out = [b for b in self.bars if b.ts_ms >= start][: self.page]
        return klines_json(out)


def http_forward(v: ForwardVenue) -> claude_worker.candles.Http:
    base = http_none()
    return claude_worker.candles.Http(get=v.get, post=base.post, hosts=base.hosts)


def test_fill_forward_backfills_pages_and_reaches_open_bar(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = bn_target()
    open_ts = (NOW // MS_1M) * MS_1M
    bars = [mk_candle(open_ts - i * MS_1M, base=100.0 + i) for i in range(7, 0, -1)]
    bars.append(mk_candle(open_ts, base=50.0))  # the open bar
    venue = ForwardVenue(bars, page=3)
    st = claude_worker.candles.fill_forward(
        conn, http_forward(venue), bn_lane(t), t, "1m", NOW, budget(10), {}
    )
    assert not st.budget_out and not st.failed
    assert st.upsert.inserted == 8
    assert claude_worker.candles.max_open_ts(conn, t.venue, t.descriptor, "1m") == open_ts
    # First request honored the 48 h §9.6 backfill bound.
    assert venue.calls[0] == NOW - 48 * MS_1H
    # Monotone frontier, no re-walk.
    assert venue.calls == sorted(venue.calls)


def test_fill_forward_resumes_from_frontier_and_closes_open_bar(
    tmp_path: pathlib.Path,
) -> None:
    conn = db(tmp_path)
    t = bn_target()
    open_ts = (NOW // MS_1M) * MS_1M
    claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, [mk_candle(open_ts, base=50.0)], NOW
    )
    later = open_ts + MS_1M + 30_000  # the bar has closed since
    venue = ForwardVenue(
        [mk_candle(open_ts, base=51.0), mk_candle(open_ts + MS_1M, base=52.0)], page=10
    )
    st = claude_worker.candles.fill_forward(
        conn, http_forward(venue), bn_lane(t), t, "1m", later, budget(10), {}
    )
    # Frontier re-requested from the stored (then-open) bar.
    assert venue.calls[0] == open_ts
    # The stored row was a mid-life snapshot (open at fetch): the
    # refetch FINALIZES it to the venue's true close — no conflict.
    assert st.upsert.conflicts == 0
    assert st.upsert.updated_open == 1
    assert st.upsert.inserted == 1  # the new open bar
    kept = conn.execute("SELECT o FROM candles WHERE open_ts=?", (open_ts,)).fetchone()
    assert kept[0] == 51.0


def test_fill_forward_budget_exhaustion_resumes_next_cycle(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = bn_target()
    open_ts = (NOW // MS_1M) * MS_1M
    bars = [mk_candle(open_ts - i * MS_1M, base=100.0 + i) for i in range(6, -1, -1)]
    venue = ForwardVenue(bars, page=2)
    st = claude_worker.candles.fill_forward(
        conn, http_forward(venue), bn_lane(t), t, "1m", NOW, budget(2), {}
    )
    assert st.budget_out and st.upsert.inserted == 4
    frontier = claude_worker.candles.max_open_ts(conn, t.venue, t.descriptor, "1m")
    assert frontier == bars[3].ts_ms
    # Next cycle, fresh budget: picks up EXACTLY at the frontier.
    venue2 = ForwardVenue(bars, page=10)
    st2 = claude_worker.candles.fill_forward(
        conn, http_forward(venue2), bn_lane(t), t, "1m", NOW, budget(10), {}
    )
    assert venue2.calls[0] == frontier
    assert not st2.budget_out
    assert claude_worker.candles.max_open_ts(conn, t.venue, t.descriptor, "1m") == open_ts


def test_fill_forward_failure_and_progress_guard(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = bn_target()
    st = claude_worker.candles.fill_forward(
        conn, http_none(), bn_lane(t), t, "1m", NOW, budget(5), {}
    )
    assert st.failed and st.pages == 0
    # Progress guard: a venue that keeps returning the same old bar.
    stuck_bar = mk_candle(NOW - 40 * MS_1H)

    def stuck_get(url: str) -> str:
        return klines_json([stuck_bar])

    base = http_none()
    http = claude_worker.candles.Http(get=stuck_get, post=base.post, hosts=base.hosts)
    st2 = claude_worker.candles.fill_forward(
        conn, http, bn_lane(t), t, "1m", NOW, budget(50), {}
    )
    assert not st2.failed and st2.pages <= 2, "no infinite loop on a stuck venue"


# ---- OKX backward gap-fill ----------------------------------------------


class OkxVenue:
    """Fake OKX history-candles honoring the REAL page contract:
    `after=` returns bars STRICTLY older, newest-first, up to the
    venue limit (100) — a short page only at the listing edge."""

    def __init__(self, bars: list[claude_worker.fetchers.Candle]) -> None:
        self.bars = sorted(bars, key=lambda x: x.ts_ms)
        self.page = claude_worker.candles.OKX_PAGE_LIMIT
        self.calls = 0

    def get(self, url: str) -> str | None:
        self.calls += 1
        after = int(url.split("after=")[1].split("&")[0])
        older = [b for b in self.bars if b.ts_ms < after]
        return okx_json(older[-self.page :])


def okx_target() -> claude_worker.candles.LaneTarget:
    return claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_OKX, "okx:BTC-USDT", "BTC-USDT"
    )


def http_okx(v: OkxVenue) -> claude_worker.candles.Http:
    base = http_none()
    return claude_worker.candles.Http(get=v.get, post=base.post, hosts=base.hosts)


def test_okx_backward_multipage_connects_to_bound(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = okx_target()
    bound = NOW - 48 * MS_1H
    # 250 bars STRADDLING the bound: 50 older, 200 in-bound. The walk
    # must page back to the bound, then keep only in-bound bars.
    bars = [mk_candle(bound + (i - 50) * MS_1M, base=100.0) for i in range(250)]
    venue = OkxVenue(bars)
    st = claude_worker.candles.fill_okx_backward(
        conn, http_okx(venue), t, "1m", NOW, budget(10), {}
    )
    assert not st.budget_out and not st.failed
    assert venue.calls == 2, "second page's oldest lands ON the bound"
    assert st.bars == 200, "bars older than the §9.6 bound dropped"
    assert st.upsert.inserted == 200
    assert (
        claude_worker.candles.max_open_ts(conn, t.venue, t.descriptor, "1m")
        == bars[-1].ts_ms
    )


def test_okx_backward_short_page_is_listing_edge(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = okx_target()
    # 40 bars total (short first page): the venue's whole history —
    # legal even though the bound is never reached.
    start = NOW - 40 * MS_1M
    bars = [mk_candle(start + i * MS_1M) for i in range(40)]
    venue = OkxVenue(bars)
    st = claude_worker.candles.fill_okx_backward(
        conn, http_okx(venue), t, "1m", NOW, budget(10), {}
    )
    assert venue.calls == 1 and st.upsert.inserted == 40


def test_okx_backward_budget_truncation_discards_whole_walk(
    tmp_path: pathlib.Path,
) -> None:
    conn = db(tmp_path)
    t = okx_target()
    bound = NOW - 48 * MS_1H
    bars = [mk_candle(bound + i * MS_1M) for i in range(250)]
    venue = OkxVenue(bars)
    st = claude_worker.candles.fill_okx_backward(
        conn, http_okx(venue), t, "1m", NOW, budget(1), {}
    )
    assert st.budget_out and st.bars == 0
    assert claude_worker.candles.max_open_ts(conn, t.venue, t.descriptor, "1m") is None, (
        "a truncated backward walk must never advance the frontier"
    )


def test_okx_backward_resumes_from_frontier(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    t = okx_target()
    first = NOW - 10 * MS_1M
    claude_worker.candles.upsert_rest(
        conn, t.venue, t.descriptor, "1m", MS_1M, [mk_candle(first)], NOW
    )
    bars = [mk_candle(first + i * MS_1M, base=200.0 + i) for i in range(4)]
    venue = OkxVenue(bars)
    st = claude_worker.candles.fill_okx_backward(
        conn, http_okx(venue), t, "1m", NOW, budget(10), {}
    )
    assert venue.calls == 1, "one page connects to the frontier"
    assert st.bars == 4, "frontier bar re-requested (open-bar law) + 3 new"


# ---- cycle + main --------------------------------------------------------


def test_run_cycle_shares_budget_per_venue(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    spot = bn_target()
    usdm = claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_BINANCE, "binance-usdm:btcusdt", "BTCUSDT"
    )
    lanes = [
        claude_worker.candles.Lane("binance", spot.venue, [spot], backward=False),
        claude_worker.candles.Lane("binance-usdm", usdm.venue, [usdm], backward=False),
    ]
    calls: list[str] = []

    def get(url: str) -> str:
        calls.append(url)
        return klines_json([])

    base = http_none()
    http = claude_worker.candles.Http(get=get, post=base.post, hosts=base.hosts)
    lines: list[str] = []
    claude_worker.candles.run_cycle(conn, lanes, http, NOW, 4, {}, lines.append)
    # ONE venue budget of 4 across both binance lanes × 3 tfs.
    assert len(calls) == 4
    assert sum("BUDGET" in line for line in lines) == 2
    assert any("binance:btcusdt 1m" in line for line in lines)


def test_main_no_lanes_and_unusable_universe(tmp_path: pathlib.Path) -> None:
    empty = tmp_path / "u.toml"
    empty.write_text("[polymarket]\nmarkets = []\n", encoding="utf-8")
    rc = claude_worker.candles.main(
        ["--universe", str(empty), "--db", str(tmp_path / "c.db"), "--now-ms", str(NOW)]
    )
    assert rc == 0
    rc2 = claude_worker.candles.main(
        ["--universe", str(tmp_path / "absent.toml"), "--db", str(tmp_path / "c.db")]
    )
    assert rc2 == 1
