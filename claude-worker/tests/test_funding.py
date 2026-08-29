# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""funding-history lane tests (WS11) — additive; frozen surfaces
untouched. No live API calls: injected transports; payloads mirror
each venue's real wire shape."""

import json
import pathlib
import sqlite3

import claude_worker.candles
import claude_worker.frames
import claude_worker.funding


NOW = 1_787_400_123_456


def db(tmp_path: pathlib.Path) -> sqlite3.Connection:
    conn = claude_worker.candles.open_db(tmp_path / "candles.db")
    claude_worker.funding.ensure_schema(conn)
    return conn


def hosts() -> dict[str, str]:
    return {
        "binance": "bn.test",
        "binance-usdm": "bnf.test",
        "okx": "okx.test",
        "deribit": "dbt.test",
        "hyperliquid": "hl.test",
        "bybit": "bybit.test",
    }


def http_map(responses: dict[str, str]) -> tuple[claude_worker.candles.Http, list[str]]:
    calls: list[str] = []

    def get(url: str) -> str | None:
        calls.append(url)
        return responses.get(url)

    def post(url: str, body: str) -> str | None:
        calls.append(url + "|" + body)
        return responses.get(url)

    return claude_worker.candles.Http(get=get, post=post, hosts=hosts()), calls


def all_rows(conn: sqlite3.Connection) -> list[tuple[object, ...]]:
    return list(
        conn.execute(
            "SELECT venue, descriptor, ts_ms, rate FROM funding ORDER BY descriptor, ts_ms"
        )
    )


# ---- lane selection ------------------------------------------------------


def test_funding_lanes_select_only_funding_bearing(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "universe.toml"
    p.write_text(
        "[binance]\nspot=[\"btcusdt\"]\nusdm=[\"ethusdt\"]\nusdm_dated=[\"btcusdt_260327\"]\n"
        "[okx]\ninstruments=[\"BTC-USDT\",\"ETH-USDT-SWAP\"]\n"
        "[deribit]\ninstruments=[\"BTC-PERPETUAL\",\"BTC_USDC\"]\n"
        "[hyperliquid]\ncoins=[\"BTC\",\"@1\",\"#330\"]\n"
        "[bybit]\nspot=[\"BTCUSDT\"]\nlinear=[\"ETHUSDT\"]\n",
        encoding="utf-8",
    )
    lanes = claude_worker.funding.read_funding_lanes(p)
    assert lanes is not None
    by_name = {lane.name: [t.instrument for t in lane.targets] for lane in lanes}
    assert by_name == {
        "binance-usdm": ["ETHUSDT"],  # the dated `_` name excluded
        "okx": ["ETH-USDT-SWAP"],  # spot instId excluded
        "deribit": ["BTC-PERPETUAL"],  # spot name excluded
        "hyperliquid": ["BTC"],  # @spot/#outcome excluded
        "bybit-linear": ["ETHUSDT"],  # bybit spot lane never appears
    }
    # Descriptors keep the §9.4 namespaces.
    lane = next(x for x in lanes if x.name == "binance-usdm")
    assert lane.targets[0].descriptor == "binance-usdm:ethusdt"
    assert claude_worker.funding.read_funding_lanes(tmp_path / "missing.toml") is None


# ---- parsers -------------------------------------------------------------


def test_parsers_cover_all_five_wire_shapes() -> None:
    bn = json.dumps(
        [
            {"symbol": "ETHUSDT", "fundingTime": 1_700_000_000_000, "fundingRate": "0.0001"},
            {"symbol": "ETHUSDT", "fundingTime": "bad", "fundingRate": "0.1"},
        ]
    )
    assert claude_worker.funding.parse_bn_funding(bn) == [(1_700_000_000_000, 0.0001)]
    okx = json.dumps(
        {
            "code": "0",
            "data": [
                {"instId": "ETH-USDT-SWAP", "fundingRate": "-0.0002", "fundingTime": "1700000000000"}
            ],
        }
    )
    assert claude_worker.funding.parse_okx_funding(okx) == [(1_700_000_000_000, -0.0002)]
    assert claude_worker.funding.parse_okx_funding(json.dumps({"code": "51001"})) is None
    dbt = json.dumps(
        {"result": [{"timestamp": 1_700_000_000_000, "interest_8h": 0.00042, "index_price": 1.0}]}
    )
    assert claude_worker.funding.parse_deribit_funding(dbt) == [(1_700_000_000_000, 0.00042)]
    hl = json.dumps(
        [{"coin": "BTC", "fundingRate": "0.0000125", "premium": "0.0003", "time": 1_700_000_000_000}]
    )
    assert claude_worker.funding.parse_hl_funding(hl) == [(1_700_000_000_000, 0.0000125)]
    bb = json.dumps(
        {
            "retCode": 0,
            "result": {
                "list": [{"symbol": "ETHUSDT", "fundingRate": "0.0001", "fundingRateTimestamp": "1700000000000"}]
            },
        }
    )
    assert claude_worker.funding.parse_bybit_funding(bb) == [(1_700_000_000_000, 0.0001)]
    for parse in (
        claude_worker.funding.parse_bn_funding,
        claude_worker.funding.parse_okx_funding,
        claude_worker.funding.parse_deribit_funding,
        claude_worker.funding.parse_hl_funding,
        claude_worker.funding.parse_bybit_funding,
    ):
        assert parse("junk") is None


# ---- store + cycle -------------------------------------------------------


def test_upsert_is_idempotent(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    venue = claude_worker.frames.VENUE_OKX
    points = [(1_700_000_000_000, 0.0001), (1_700_028_800_000, -0.0002)]
    assert claude_worker.funding.upsert_points(conn, venue, "okx:X-SWAP", points, NOW) == 2
    # Re-fetching the same page adds nothing (immutable history).
    assert claude_worker.funding.upsert_points(conn, venue, "okx:X-SWAP", points, NOW) == 0
    assert len(all_rows(conn)) == 2


def test_cycle_fetches_and_stores_per_lane(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    lanes = [
        claude_worker.funding.FundingLane(
            "okx",
            claude_worker.frames.VENUE_OKX,
            [
                claude_worker.funding.FundingTarget(
                    claude_worker.frames.VENUE_OKX, "okx:ETH-USDT-SWAP", "ETH-USDT-SWAP"
                )
            ],
        )
    ]
    http, calls = http_map(
        {
            "https://okx.test/api/v5/public/funding-rate-history?instId=ETH-USDT-SWAP&limit=100": json.dumps(
                {
                    "code": "0",
                    "data": [
                        {"fundingRate": "0.0001", "fundingTime": "1700000000000"},
                        {"fundingRate": "0.0002", "fundingTime": "1699971200000"},
                    ],
                }
            )
        }
    )
    lines: list[str] = []
    claude_worker.funding.run_cycle(conn, lanes, http, NOW, 30, lines.append)
    assert len(calls) == 1
    rows = all_rows(conn)
    assert len(rows) == 2
    assert rows[0][1] == "okx:ETH-USDT-SWAP"
    assert any("okx: targets=1 points=2 +2 failed=0" in line for line in lines)
    # A second cycle re-fetches but adds nothing.
    claude_worker.funding.run_cycle(conn, lanes, http, NOW + 1, 30, lines.append)
    assert any("+0 failed=0" in line for line in lines)


def test_cycle_budget_and_failures(tmp_path: pathlib.Path) -> None:
    conn = db(tmp_path)
    targets = [
        claude_worker.funding.FundingTarget(
            claude_worker.frames.VENUE_BYBIT, f"bybit-linear:C{i}USDT", f"C{i}USDT"
        )
        for i in range(3)
    ]
    lanes = [
        claude_worker.funding.FundingLane("bybit-linear", claude_worker.frames.VENUE_BYBIT, targets)
    ]
    http, calls = http_map({})  # every fetch fails
    lines: list[str] = []
    claude_worker.funding.run_cycle(conn, lanes, http, NOW, 2, lines.append)
    assert len(calls) == 2, "third refused by the budget"
    assert any("failed=2 BUDGET" in line for line in lines)
    assert all_rows(conn) == []


def test_hl_fetch_resumes_from_newest_stored_point(tmp_path: pathlib.Path) -> None:
    """M5-onboarding pin (2026-08-29): HL fundingHistory pages ASCENDING
    from startTime — a fixed now-33d start returns the OLDEST page every
    cycle and the series stalls ~12 days behind. The cycle must resume
    from the newest stored point + 1."""
    conn = db(tmp_path)
    target = claude_worker.funding.FundingTarget(
        claude_worker.frames.VENUE_HYPERLIQUID, "hyperliquid:ADA", "ADA"
    )
    lanes = [
        claude_worker.funding.FundingLane(
            "hyperliquid", claude_worker.frames.VENUE_HYPERLIQUID, [target]
        )
    ]
    stored_ts = NOW - 5 * 86_400_000
    claude_worker.funding.upsert_points(
        conn, target.venue, target.descriptor, [(stored_ts, 1e-5)], NOW
    )

    seen_bodies: list[str] = []

    class Http:
        hosts = {"hyperliquid": "api.hyperliquid.xyz"}

        def get(self, url: str) -> str | None:
            raise AssertionError("HL lane must POST")

        def post(self, url: str, body: str) -> str | None:
            seen_bodies.append(body)
            return "[]"

    claude_worker.funding.run_cycle(conn, lanes, Http(), NOW, 30, lambda _s: None)
    assert len(seen_bodies) == 1
    body = json.loads(seen_bodies[0])
    assert body["startTime"] == stored_ts + 1, "must resume past the stored point"
