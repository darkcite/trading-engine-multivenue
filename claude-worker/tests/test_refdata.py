# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""refdata lane tests (WS4) — additive; frozen verb surface + pinned
suites untouched. No live API calls: transports are injected fakes;
venue payloads mirror the real wire shapes (Binance ticker/24hr +
openInterest, Deribit get_book_summary_by_instrument)."""

import json
import pathlib
import sqlite3

import claude_worker.candles
import claude_worker.features
import claude_worker.fetchers
import claude_worker.frames
import claude_worker.refdata


MS_1H = claude_worker.refdata.MS_1H
NOW = 1_787_400_123_456  # mid-hour wall ms


def db(tmp_path: pathlib.Path) -> sqlite3.Connection:
    conn = claude_worker.candles.open_db(tmp_path / "candles.db")
    claude_worker.refdata.ensure_schema(conn)
    return conn


def hosts() -> dict[str, str]:
    return {
        "binance": "bn.test",
        "binance-usdm": "bnf.test",
        "okx": "okx.test",
        "deribit": "dbt.test",
        "hyperliquid": "hl.test",
        "bybit": "bybit.test",
        "mexc": "mx.test",
        "mexc-perp": "mxf.test",
    }


def http_map(responses: dict[str, str]) -> tuple[claude_worker.candles.Http, list[str]]:
    """Http fake serving exact-URL responses; records every GET."""
    calls: list[str] = []

    def get(url: str) -> str | None:
        calls.append(url)
        return responses.get(url)

    return (
        claude_worker.candles.Http(get=get, post=lambda url, body: None, hosts=hosts()),
        calls,
    )


def report_sink() -> tuple[list[str], object]:
    lines: list[str] = []
    return lines, lines.append


def lanes_bn_spot() -> list[claude_worker.candles.Lane]:
    target = claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_BINANCE, "binance:btcusdt", "BTCUSDT"
    )
    return [
        claude_worker.candles.Lane(
            "binance", claude_worker.frames.VENUE_BINANCE, [target], backward=False
        )
    ]


def lanes_bn_usdm() -> list[claude_worker.candles.Lane]:
    target = claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_BINANCE, "binance-usdm:btcusdt", "BTCUSDT"
    )
    return [
        claude_worker.candles.Lane(
            "binance-usdm", claude_worker.frames.VENUE_BINANCE, [target], backward=False
        )
    ]


def lanes_deribit() -> list[claude_worker.candles.Lane]:
    target = claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_DERIBIT, "deribit:BTC-PERPETUAL", "BTC-PERPETUAL"
    )
    return [
        claude_worker.candles.Lane(
            "deribit", claude_worker.frames.VENUE_DERIBIT, [target], backward=False
        )
    ]


def all_rows(conn: sqlite3.Connection) -> list[tuple[object, ...]]:
    return list(
        conn.execute(
            "SELECT venue, descriptor, kind, hour_ts, value FROM refdata"
            " ORDER BY venue, descriptor, kind"
        )
    )


# ---- parsers -------------------------------------------------------------


def test_parse_bn_ticker24h_extracts_quote_volume() -> None:
    raw = json.dumps(
        {"symbol": "BTCUSDT", "quoteVolume": "123456.789", "volume": "1.5", "count": 42}
    )
    assert claude_worker.refdata.parse_bn_ticker24h(raw) == 123456.789
    assert claude_worker.refdata.parse_bn_ticker24h("not json") is None
    assert claude_worker.refdata.parse_bn_ticker24h("[]") is None
    assert claude_worker.refdata.parse_bn_ticker24h('{"quoteVolume": 1.0}') is None
    assert claude_worker.refdata.parse_bn_ticker24h('{"quoteVolume": "junk"}') is None


def test_parse_bn_open_interest() -> None:
    raw = json.dumps({"openInterest": "10659.509", "symbol": "BTCUSDT", "time": 1})
    assert claude_worker.refdata.parse_bn_open_interest(raw) == 10659.509
    assert claude_worker.refdata.parse_bn_open_interest("{}") is None


def test_parse_deribit_book_summary() -> None:
    raw = json.dumps(
        {
            "jsonrpc": "2.0",
            "result": [
                {
                    "volume_usd": 219994260.0,
                    "volume": 3399.9,
                    "open_interest": 686269060,
                    "mark_price": 64700.5,
                }
            ],
        }
    )
    assert claude_worker.refdata.parse_deribit_book_summary(raw) == (
        219994260.0,
        686269060.0,
    )
    # Partial rows: missing elements come back None, not a failure.
    partial = json.dumps({"result": [{"volume": 1.0}]})
    assert claude_worker.refdata.parse_deribit_book_summary(partial) == (None, None)
    assert claude_worker.refdata.parse_deribit_book_summary("junk") is None
    assert claude_worker.refdata.parse_deribit_book_summary('{"result": []}') is None
    # Bool masquerading as a number is rejected (json true is not OI).
    boolish = json.dumps({"result": [{"open_interest": True, "volume_usd": 1.0}]})
    assert claude_worker.refdata.parse_deribit_book_summary(boolish) == (1.0, None)


# ---- store semantics -----------------------------------------------------


def test_snapshot_upsert_buckets_by_hour_and_replaces(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    venue = claude_worker.frames.VENUE_BINANCE
    claude_worker.refdata.upsert_snapshot(conn, venue, "binance:btcusdt", "oi", NOW, 1.0)
    # Second write in the SAME hour replaces in place.
    claude_worker.refdata.upsert_snapshot(
        conn, venue, "binance:btcusdt", "oi", NOW + 60_000, 2.0
    )
    # Next hour = a new row.
    claude_worker.refdata.upsert_snapshot(
        conn, venue, "binance:btcusdt", "oi", NOW + MS_1H, 3.0
    )
    conn.commit()
    rows = list(
        conn.execute("SELECT hour_ts, value FROM refdata ORDER BY hour_ts").fetchall()
    )
    assert len(rows) == 2
    assert rows[0][0] == (NOW // MS_1H) * MS_1H
    assert rows[0][1] == 2.0, "latest write within the hour wins"
    assert rows[1][1] == 3.0


def test_refdata_rides_beside_candles_in_one_store(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    tables = {
        row[0]
        for row in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")
    }
    assert "candles" in tables and "refdata" in tables


# ---- cycle ---------------------------------------------------------------


def test_cycle_bn_spot_stores_vol24h(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    http, calls = http_map(
        {
            "https://bn.test/api/v3/ticker/24hr?symbol=BTCUSDT": json.dumps(
                {"quoteVolume": "5000.5"}
            )
        }
    )
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes_bn_spot(), http, NOW, 30, report)
    rows = all_rows(conn)
    assert rows == [
        (
            claude_worker.frames.VENUE_BINANCE,
            "binance:btcusdt",
            "vol24h_quote",
            (NOW // MS_1H) * MS_1H,
            5000.5,
        )
    ]
    assert len(calls) == 1
    assert any("binance: targets=1 rows=1 failed=0" in line for line in lines)


def test_cycle_bn_usdm_stores_vol_and_oi(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    http, calls = http_map(
        {
            "https://bnf.test/fapi/v1/ticker/24hr?symbol=BTCUSDT": json.dumps(
                {"quoteVolume": "9e6"}
            ),
            "https://bnf.test/fapi/v1/openInterest?symbol=BTCUSDT": json.dumps(
                {"openInterest": "10659.5"}
            ),
        }
    )
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes_bn_usdm(), http, NOW, 30, report)
    kinds = {(r[2], r[4]) for r in all_rows(conn)}
    assert kinds == {("vol24h_quote", 9e6), ("oi", 10659.5)}
    assert len(calls) == 2


def test_cycle_deribit_one_call_two_kinds(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    http, calls = http_map(
        {
            "https://dbt.test/api/v2/public/get_book_summary_by_instrument"
            "?instrument_name=BTC-PERPETUAL": json.dumps(
                {"result": [{"volume_usd": 219994260.0, "open_interest": 686269060.0}]}
            )
        }
    )
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes_deribit(), http, NOW, 30, report)
    kinds = {(r[2], r[4]) for r in all_rows(conn)}
    assert kinds == {("vol24h_quote", 219994260.0), ("oi", 686269060.0)}
    assert len(calls) == 1, "one body carries both series"


def test_cycle_transport_failure_counts_and_continues(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    http, _calls = http_map({})  # every GET -> None
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes_bn_spot(), http, NOW, 30, report)
    assert all_rows(conn) == []
    assert any("failed=1" in line for line in lines)


def test_cycle_budget_exhaustion_stops_lane(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    targets = [
        claude_worker.candles.LaneTarget(
            claude_worker.frames.VENUE_BINANCE, f"binance:c{i}", f"C{i}USDT"
        )
        for i in range(3)
    ]
    lanes = [
        claude_worker.candles.Lane(
            "binance", claude_worker.frames.VENUE_BINANCE, targets, backward=False
        )
    ]
    responses = {
        f"https://bn.test/api/v3/ticker/24hr?symbol=C{i}USDT": json.dumps(
            {"quoteVolume": "1.0"}
        )
        for i in range(3)
    }
    http, calls = http_map(responses)
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes, http, NOW, 2, report)
    assert len(calls) == 2, "third call refused by the budget"
    assert len(all_rows(conn)) == 2
    assert any("BUDGET" in line for line in lines)


def test_parse_okx_bodies() -> None:
    ok = json.dumps({"code": "0", "data": [{"volCcy24h": "123.5", "vol24h": "9"}]})
    assert claude_worker.refdata.parse_okx_ticker24h(ok) == 123.5
    err = json.dumps({"code": "51001", "msg": "instId not exist", "data": []})
    assert claude_worker.refdata.parse_okx_ticker24h(err) is None
    oi = json.dumps({"code": "0", "data": [{"oi": "5000", "oiCcy": "50.0"}]})
    assert claude_worker.refdata.parse_okx_open_interest(oi) == 5000.0
    assert claude_worker.refdata.parse_okx_open_interest("junk") is None


def test_parse_hl_asset_ctxs_aligns_ordinals() -> None:
    raw = json.dumps(
        [
            {"universe": [{"name": "BTC", "szDecimals": 5}, {"name": "ETH", "szDecimals": 4}]},
            [{"dayNtlVlm": "1169046.29", "funding": "0.00001"}, {"funding": "0.0"}],
        ]
    )
    vols = claude_worker.refdata.parse_hl_asset_ctxs(raw)
    assert vols == {"BTC": 1169046.29}, "ETH ctx lacks dayNtlVlm — skipped"
    assert claude_worker.refdata.parse_hl_asset_ctxs("[]") is None
    assert claude_worker.refdata.parse_hl_asset_ctxs("junk") is None


def test_cycle_okx_vol_always_oi_derivatives_only(tmp_path: pathlib.Path) -> None:
    # WS7: spot instIds (2 segments) get vol only; swaps get vol + OI.
    conn = db(tmp_path)
    targets = [
        claude_worker.candles.LaneTarget(
            claude_worker.frames.VENUE_OKX, "okx:BTC-USDT", "BTC-USDT"
        ),
        claude_worker.candles.LaneTarget(
            claude_worker.frames.VENUE_OKX, "okx:ETH-USDT-SWAP", "ETH-USDT-SWAP"
        ),
    ]
    lanes = [claude_worker.candles.Lane("okx", claude_worker.frames.VENUE_OKX, targets, backward=True)]
    http, calls = http_map(
        {
            "https://okx.test/api/v5/market/ticker?instId=BTC-USDT": json.dumps(
                {"code": "0", "data": [{"volCcy24h": "111.0"}]}
            ),
            "https://okx.test/api/v5/market/ticker?instId=ETH-USDT-SWAP": json.dumps(
                {"code": "0", "data": [{"volCcy24h": "222.0"}]}
            ),
            "https://okx.test/api/v5/public/open-interest?instId=ETH-USDT-SWAP": json.dumps(
                {"code": "0", "data": [{"oi": "3333.0"}]}
            ),
        }
    )
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes, http, NOW, 30, report)
    assert len(calls) == 3, "no OI call for the spot instId"
    rows = {(r[1], r[2], r[4]) for r in all_rows(conn)}
    assert rows == {
        ("okx:BTC-USDT", "vol24h_quote", 111.0),
        ("okx:ETH-USDT-SWAP", "vol24h_quote", 222.0),
        ("okx:ETH-USDT-SWAP", "oi", 3333.0),
    }
    assert any("okx: targets=2 rows=3 failed=0" in line for line in lines)


def test_cycle_hl_one_call_perps_only(tmp_path: pathlib.Path) -> None:
    # WS8: one metaAndAssetCtxs body serves every perp coin; @spot and
    # #outcome coins are skipped without a call.
    conn = db(tmp_path)
    targets = [
        claude_worker.candles.LaneTarget(
            claude_worker.frames.VENUE_HYPERLIQUID, "hyperliquid:BTC", "BTC"
        ),
        claude_worker.candles.LaneTarget(
            claude_worker.frames.VENUE_HYPERLIQUID, "hyperliquid:@1", "@1"
        ),
        claude_worker.candles.LaneTarget(
            claude_worker.frames.VENUE_HYPERLIQUID, "hyperliquid:#330", "#330"
        ),
    ]
    lanes = [
        claude_worker.candles.Lane(
            "hyperliquid", claude_worker.frames.VENUE_HYPERLIQUID, targets, backward=False
        )
    ]
    posts: list[tuple[str, str]] = []

    def post(url: str, body: str) -> str | None:
        posts.append((url, body))
        return json.dumps(
            [
                {"universe": [{"name": "BTC", "szDecimals": 5}]},
                [{"dayNtlVlm": "999.5"}],
            ]
        )

    http = claude_worker.candles.Http(get=lambda url: None, post=post, hosts=hosts())
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes, http, NOW, 30, report)
    assert len(posts) == 1, "one body for the whole lane"
    assert posts[0][0] == "https://hl.test/info"
    assert '"type":"metaAndAssetCtxs"' in posts[0][1]
    rows = all_rows(conn)
    assert len(rows) == 1
    assert rows[0][1] == "hyperliquid:BTC"
    assert rows[0][4] == 999.5
    assert any("hyperliquid: targets=3 rows=1 failed=0" in line for line in lines)


def test_cycle_bybit_linear_one_call_vol_and_oi(tmp_path: pathlib.Path) -> None:
    # WS9: one tickers body per target — spot stores vol only, linear
    # stores vol + OI.
    conn = db(tmp_path)
    spot_t = claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_BYBIT, "bybit:BTCUSDT", "BTCUSDT"
    )
    lin_t = claude_worker.candles.LaneTarget(
        claude_worker.frames.VENUE_BYBIT, "bybit-linear:BTCUSDT", "BTCUSDT"
    )
    lanes = [
        claude_worker.candles.Lane("bybit", claude_worker.frames.VENUE_BYBIT, [spot_t], backward=False),
        claude_worker.candles.Lane(
            "bybit-linear", claude_worker.frames.VENUE_BYBIT, [lin_t], backward=False
        ),
    ]
    http, calls = http_map(
        {
            "https://bybit.test/v5/market/tickers?category=spot&symbol=BTCUSDT": json.dumps(
                {"retCode": 0, "result": {"list": [{"turnover24h": "111.5"}]}}
            ),
            "https://bybit.test/v5/market/tickers?category=linear&symbol=BTCUSDT": json.dumps(
                {
                    "retCode": 0,
                    "result": {
                        "list": [{"turnover24h": "222.5", "openInterest": "68744.761"}]
                    },
                }
            ),
        }
    )
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes, http, NOW, 30, report)
    assert len(calls) == 2
    rows = {(r[1], r[2], r[4]) for r in all_rows(conn)}
    assert rows == {
        ("bybit:BTCUSDT", "vol24h_quote", 111.5),
        ("bybit-linear:BTCUSDT", "vol24h_quote", 222.5),
        ("bybit-linear:BTCUSDT", "oi", 68744.761),
    }
    # retCode != 0 is an unusable body.
    err = json.dumps({"retCode": 10001, "result": {}})
    assert claude_worker.refdata.parse_bybit_ticker(err) is None


#: MX7, measured live 2026-09-23 (trimmed): `GET contract.mexc.com/api/v1/
#: contract/ticker?symbol=XAU_USDT` — BARE numbers, `data` an object.
MEXC_PERP_TICKER = (
    '{"success":true,"code":0,"data":{"contractId":1935,"symbol":"XAU_USDT",'
    '"lastPrice":4332.05,"bid1":4332.05,"ask1":4332.08,"volume24":72391669,'
    '"amount24":313985661.21342,"holdVol":82300176,"indexPrice":4329.61,'
    '"fairPrice":4331.97,"fundingRate":0.000059,"timestamp":1790144201736}}'
)
#: `GET api.mexc.com/api/v3/ticker/24hr?symbol=BTCUSDT` (trimmed) — the
#: Binance body, quoted decimals.
MEXC_SPOT_TICKER = (
    '{"symbol":"BTCUSDT","lastPrice":"86601.26","volume":"10102.14266626",'
    '"quoteVolume":"870963232.08","openTime":1790144201042,"closeTime":1790144205082,'
    '"count":null}'
)


def test_parse_mexc_ticker_live_shape_and_failures() -> None:
    parse = claude_worker.refdata.parse_mexc_ticker
    assert parse(MEXC_PERP_TICKER) == (313985661.21342, 82300176.0)
    assert parse('{"success":false,"code":1001,"message":"Contract does not exist"}') is None
    assert parse("junk") is None
    assert parse('{"success":true,"code":0,"data":[]}') is None
    # A quoted or boolean field is absent, never coerced.
    assert parse('{"success":true,"code":0,"data":{"amount24":"1","holdVol":true}}') == (
        None,
        None,
    )
    # The spot 24 h ticker is the Binance body, read by the Binance parser.
    assert claude_worker.refdata.parse_bn_ticker24h(MEXC_SPOT_TICKER) == 870963232.08


def test_cycle_mexc_spot_vol_and_perp_vol_plus_oi(tmp_path: pathlib.Path) -> None:
    # MX7: each class on its own host — spot stores vol only, the perp
    # contract ticker stores vol + OI from one body.
    conn = db(tmp_path)
    mexc = claude_worker.frames.VENUE_MEXC
    spot_t = claude_worker.candles.LaneTarget(mexc, "mexc:BTCUSDT", "BTCUSDT")
    perp_t = claude_worker.candles.LaneTarget(mexc, "mexc-perp:XAU_USDT", "XAU_USDT")
    lanes = [
        claude_worker.candles.Lane("mexc", mexc, [spot_t], backward=False),
        claude_worker.candles.Lane("mexc-perp", mexc, [perp_t], backward=False),
    ]
    http, calls = http_map(
        {
            "https://mx.test/api/v3/ticker/24hr?symbol=BTCUSDT": MEXC_SPOT_TICKER,
            "https://mxf.test/api/v1/contract/ticker?symbol=XAU_USDT": MEXC_PERP_TICKER,
        }
    )
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes, http, NOW, 30, report)
    assert len(calls) == 2
    rows = {(r[1], r[2], r[4]) for r in all_rows(conn)}
    assert rows == {
        ("mexc:BTCUSDT", "vol24h_quote", 870963232.08),
        ("mexc-perp:XAU_USDT", "vol24h_quote", 313985661.21342),
        ("mexc-perp:XAU_USDT", "oi", 82300176.0),
    }
    # A venue error envelope is a counted failure, not a crash.
    http, _ = http_map({})
    lines, report = report_sink()
    claude_worker.refdata.run_cycle(conn, lanes, http, NOW, 30, report)
    assert any("mexc: targets=1 rows=0 failed=1" in line for line in lines)
    assert any("mexc-perp: targets=1 rows=0 failed=1" in line for line in lines)


# ---- main shim -----------------------------------------------------------


def test_main_unusable_universe_exits_1(tmp_path: pathlib.Path) -> None:
    missing = tmp_path / "nope.toml"
    rc = claude_worker.refdata.main(
        ["--universe", str(missing), "--db", str(tmp_path / "c.db"), "--now-ms", str(NOW)]
    )
    assert rc == 1
