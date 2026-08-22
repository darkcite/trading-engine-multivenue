"""fetchers.py: the §6.1 venue REST consumers + §6.2 market-map
ownership (8h H3; design §12 fetchers/market-map rows).

Everything is fixture-driven at the injected ``get_fn``/``post_fn`` seam
— NO live API calls anywhere (house rule). The committed
``tests/fixtures/pmlr/ticks_v2.pmlr`` capture carries the universe
``{7: polymarket, 67119674: hl, 67119675: hl}``.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import os
import pathlib
import shutil
import typing

import httpx
import pytest
import typer.testing

import claude_worker.cli
import claude_worker.features
import claude_worker.fetchers
import claude_worker.frames
import tests.conftest

FIXTURES = pathlib.Path(__file__).parent / "fixtures" / "pmlr"
_RUN_NAME = "run-100"

PM7 = 7
HL_YES = (4 << 24) + 10_810
HL_NO = (4 << 24) + 10_811

# A 10-digit run — exactly PM_TOKEN_RUN_MIN, the shortest accepted id.
TOK_A = "1111111111"
TOK_B = "2222222222"

GOOD_GAMMA = json.dumps(
    [
        {
            "question": "Xi Jinping out before 2027?",
            "slug": "xi-out-2027",
            "clobTokenIds": f'["{TOK_A}", "{TOK_B}"]',
            "outcomes": '["Yes", "No"]',
            "liquidity": "48123.4",  # extra keys tolerated (external API)
        }
    ]
)

GOOD_OKX = json.dumps(
    {
        "code": "0",
        "msg": "",
        "data": [
            ["1700000060000", "2.0", "2.5", "1.5", "2.2", "11", "x", "y", "1"],
            ["1700000000000", "1.0", "1.5", "0.5", "1.2", "10", "x", "y", "1"],
        ],
    }
)

GOOD_DERIBIT = json.dumps(
    {
        "jsonrpc": "2.0",
        "result": {
            "status": "ok",
            "ticks": [1700000000000, 1700000060000],
            "open": [1.0, 2.0],
            "high": [1.5, 2.5],
            "low": [0.5, 1.5],
            "close": [1.2, 2.2],
            "volume": [10, 11],
            "cost": [1, 2],
        },
    }
)

GOOD_HL = json.dumps(
    [
        {"t": 1700000000000, "T": 1700000059999, "s": "BTC", "i": "1m",
         "o": "1.0", "h": "1.5", "l": "0.5", "c": "1.2", "v": "10", "n": 3},
        {"t": 1700000060000, "T": 1700000119999, "s": "BTC", "i": "1m",
         "o": "2.0", "h": "2.5", "l": "1.5", "c": "2.2", "v": "11", "n": 4},
    ]
)


def _mk_run(root: pathlib.Path) -> pathlib.Path:
    run = root / _RUN_NAME
    run.mkdir(parents=True)
    shutil.copy(FIXTURES / "ticks_v2.pmlr", run / "pm-ticks.pmlr")
    return run


def _budget(max_calls: int) -> claude_worker.features.RestBudget:
    return claude_worker.features.RestBudget(
        max_calls, claude_worker.fetchers.BUDGET_WINDOW_NS
    )


def _raise_get(url: str) -> str | None:
    raise AssertionError(f"get_fn must not be called: {url}")


def _raise_post(url: str, body: str) -> str | None:
    raise AssertionError(f"post_fn must not be called: {url}")


# ---- budget env ------------------------------------------------------------


def test_rest_budget_per_h_default_and_override() -> None:
    assert claude_worker.fetchers.rest_budget_per_h(env={}) == 60
    assert claude_worker.fetchers.rest_budget_per_h(
        env={"CLAUDE_WORKER_REST_BUDGET_PER_H": "5"}
    ) == 5
    assert claude_worker.fetchers.rest_budget_per_h(
        env={"CLAUDE_WORKER_REST_BUDGET_PER_H": "0"}
    ) == 0


def test_rest_budget_per_h_malformed_is_usage_error() -> None:
    with pytest.raises(ValueError, match="integer"):
        claude_worker.fetchers.rest_budget_per_h(
            env={"CLAUDE_WORKER_REST_BUDGET_PER_H": "sixty"}
        )
    with pytest.raises(ValueError, match=">= 0"):
        claude_worker.fetchers.rest_budget_per_h(
            env={"CLAUDE_WORKER_REST_BUDGET_PER_H": "-1"}
        )


def test_rest_budget_window_reset() -> None:
    clock = [0]
    budget = claude_worker.features.RestBudget(
        1, claude_worker.fetchers.BUDGET_WINDOW_NS, clock_ns=lambda: clock[0]
    )
    assert budget.try_acquire()
    assert not budget.try_acquire()
    assert budget.skipped_total == 1
    clock[0] = claude_worker.fetchers.BUDGET_WINDOW_NS  # window rolls
    assert budget.try_acquire()


def test_venue_budgets_are_per_venue_instances() -> None:
    budgets = claude_worker.fetchers.venue_budgets(1)
    assert sorted(budgets) == [0, 2, 3, 4]  # pm + okx + deribit + hl
    assert budgets[0].try_acquire()
    assert not budgets[0].try_acquire()
    assert budgets[2].try_acquire()  # okx unaffected by pm exhaustion


# ---- observed universe -----------------------------------------------------


def test_observed_universe_from_committed_fixture(tmp_path: pathlib.Path) -> None:
    run = _mk_run(tmp_path)
    universe = claude_worker.fetchers.observed_universe(run)
    assert universe == {PM7: 0, HL_YES: 4, HL_NO: 4}


def test_observed_universe_empty_run(tmp_path: pathlib.Path) -> None:
    run = tmp_path / "run-1"
    run.mkdir()
    assert claude_worker.fetchers.observed_universe(run) == {}


# ---- strict parsers --------------------------------------------------------


def test_parse_gamma_happy() -> None:
    parsed = claude_worker.fetchers.parse_gamma_markets(GOOD_GAMMA)
    assert parsed is not None
    rows, malformed = parsed
    assert malformed == 0
    assert rows == [
        claude_worker.fetchers.GammaMarket(
            question="Xi Jinping out before 2027?",
            slug="xi-out-2027",
            token_ids=(TOK_A, TOK_B),
            outcomes=("Yes", "No"),
        )
    ]


def test_parse_gamma_malformed_rows_skipped() -> None:
    body = json.dumps(
        [
            {"slug": "no-question", "clobTokenIds": f'["{TOK_A}"]'},
            {"question": "Q?", "slug": "bad-ids", "clobTokenIds": '["12345"]'},
            {"question": "Q?", "slug": "not-json-ids", "clobTokenIds": "&&&"},
            {"question": "OK?", "slug": "ok", "clobTokenIds": f'["{TOK_B}"]'},
        ]
    )
    parsed = claude_worker.fetchers.parse_gamma_markets(body)
    assert parsed is not None
    rows, malformed = parsed
    assert malformed == 3
    assert len(rows) == 1
    assert rows[0].slug == "ok"
    assert rows[0].outcomes == ()  # absent outcomes -> empty enrichment


def test_parse_gamma_unusable_body() -> None:
    assert claude_worker.fetchers.parse_gamma_markets("not json") is None
    assert claude_worker.fetchers.parse_gamma_markets('{"a": 1}') is None


def test_parse_okx_happy_normalizes_oldest_first() -> None:
    parsed = claude_worker.fetchers.parse_okx_candles(GOOD_OKX)
    assert parsed is not None
    candles, malformed = parsed
    assert malformed == 0
    assert [c.ts_ms for c in candles] == [1700000000000, 1700000060000]
    assert candles[0] == claude_worker.fetchers.Candle(
        ts_ms=1700000000000, open=1.0, high=1.5, low=0.5, close=1.2, volume=10.0
    )


def test_parse_okx_rejects_and_row_skips() -> None:
    assert claude_worker.fetchers.parse_okx_candles("garbage") is None
    assert claude_worker.fetchers.parse_okx_candles('{"code": "1", "data": []}') is None
    assert claude_worker.fetchers.parse_okx_candles('{"code": "0"}') is None
    body = json.dumps(
        {
            "code": "0",
            "data": [
                ["1700000000000", "1.0", "1.5", "0.5", "1.2", "10"],
                ["short", "1.0"],
                [True, "1.0", "1.5", "0.5", "1.2", "10"],
            ],
        }
    )
    parsed = claude_worker.fetchers.parse_okx_candles(body)
    assert parsed is not None
    candles, malformed = parsed
    assert len(candles) == 1
    assert malformed == 2


def test_parse_deribit_happy() -> None:
    candles = claude_worker.fetchers.parse_deribit_chart(GOOD_DERIBIT)
    assert candles is not None
    assert [c.ts_ms for c in candles] == [1700000000000, 1700000060000]
    assert candles[1].close == 2.2
    assert candles[1].volume == 11.0


def test_parse_deribit_rejects_whole_body() -> None:
    assert claude_worker.fetchers.parse_deribit_chart("nope") is None
    no_data = json.dumps({"result": {"status": "no_data"}})
    assert claude_worker.fetchers.parse_deribit_chart(no_data) is None
    obj = json.loads(GOOD_DERIBIT)
    obj["result"]["close"] = [1.2]  # length mismatch
    assert claude_worker.fetchers.parse_deribit_chart(json.dumps(obj)) is None
    obj = json.loads(GOOD_DERIBIT)
    obj["result"]["open"][0] = True  # bool-rejecting coercion
    assert claude_worker.fetchers.parse_deribit_chart(json.dumps(obj)) is None


def test_parse_hl_happy() -> None:
    parsed = claude_worker.fetchers.parse_hl_candles(GOOD_HL)
    assert parsed is not None
    candles, malformed = parsed
    assert malformed == 0
    assert [c.ts_ms for c in candles] == [1700000000000, 1700000060000]
    assert candles[0].open == 1.0
    assert candles[1].volume == 11.0


def test_parse_hl_row_skips_and_unusable() -> None:
    assert claude_worker.fetchers.parse_hl_candles('{"not": "a list"}') is None
    body = json.dumps(
        [
            {"t": True, "o": "1", "h": "1", "l": "1", "c": "1", "v": "1"},
            {"t": 1700000000000, "o": "1", "h": "1", "l": "1", "c": "1"},
            {"t": 1700000000000, "o": "1", "h": "1", "l": "1", "c": "1", "v": "1"},
        ]
    )
    parsed = claude_worker.fetchers.parse_hl_candles(body)
    assert parsed is not None
    candles, malformed = parsed
    assert len(candles) == 1
    assert malformed == 2


# ---- descriptor derivation -------------------------------------------------


def test_pm_seed_kind_classification() -> None:
    kind = claude_worker.fetchers._pm_seed_kind
    assert kind(TOK_A) == "token"
    assert kind("xi-out-2027") == "slug"
    assert kind("Xi Jinping out before 2027?") is None  # resolved question
    assert kind("okx:BTC-USDT") is None  # venue-prefixed
    assert kind("123") is None  # sub-threshold digit run
    assert kind("1" * 81) is None  # over PM_TOKEN_MAX


def test_derive_targets() -> None:
    markets = {
        TOK_A: PM7,  # token seed
        "xi-out-2027": PM7,  # second seed, same sym -> first sorted wins
        "Xi Jinping out before 2027?": PM7,  # question, never a seed
        "hyperliquid:BTC": HL_YES,
        "okx:BTC-USDT": 999,  # not in universe -> dropped
        "binance:btcusdt": 42,  # not in universe -> dropped
    }
    universe = {PM7: 0, HL_YES: 4, HL_NO: 4}
    targets = claude_worker.fetchers.derive_targets(markets, universe)
    assert targets.pm_seeds == [(PM7, TOK_A)]  # "111..." sorts before "xi-"
    assert targets.candles == [(4, HL_YES, "BTC")]


def test_derive_targets_binance_never_a_candle_target() -> None:
    targets = claude_worker.fetchers.derive_targets({"binance:btcusdt": 7}, {7: 1})
    assert targets.pm_seeds == []
    assert targets.candles == []


# ---- consumers -------------------------------------------------------------


def test_fetch_pm_gamma_token_and_slug_urls_and_budget() -> None:
    urls: list[str] = []

    def get_fn(url: str) -> str | None:
        urls.append(url)
        return GOOD_GAMMA

    budget = _budget(1)
    resolved, stats = claude_worker.fetchers.fetch_pm_gamma(
        budget, get_fn, "gamma.test", [(PM7, TOK_A), (99, "xi-out-2027")]
    )
    assert urls == [f"https://gamma.test/markets?clob_token_ids={TOK_A}"]
    assert stats == claude_worker.fetchers.FetchStats(2, 1, 1, 0, 0)
    assert budget.skipped_total == 1
    assert resolved[PM7].question == "Xi Jinping out before 2027?"

    urls.clear()
    resolved, stats = claude_worker.fetchers.fetch_pm_gamma(
        _budget(1), get_fn, "gamma.test", [(PM7, "xi-out-2027")]
    )
    assert urls == ["https://gamma.test/markets?slug=xi-out-2027"]
    assert resolved[PM7].slug == "xi-out-2027"


def test_fetch_pm_gamma_failed_malformed_and_no_match() -> None:
    resolved, stats = claude_worker.fetchers.fetch_pm_gamma(
        _budget(9), lambda url: None, "h", [(PM7, TOK_A)]
    )
    assert resolved == {} and stats.failed == 1

    resolved, stats = claude_worker.fetchers.fetch_pm_gamma(
        _budget(9), lambda url: "not json", "h", [(PM7, TOK_A)]
    )
    assert resolved == {} and stats.malformed == 1

    # A well-formed body with no row containing the seed token: failed.
    resolved, stats = claude_worker.fetchers.fetch_pm_gamma(
        _budget(9), lambda url: GOOD_GAMMA, "h", [(PM7, "9999999999")]
    )
    assert resolved == {} and stats.failed == 1


def test_fetch_okx_candles_url_and_result() -> None:
    urls: list[str] = []

    def get_fn(url: str) -> str | None:
        urls.append(url)
        return GOOD_OKX

    series, stats = claude_worker.fetchers.fetch_venue_candles(
        claude_worker.frames.VENUE_OKX, _budget(9), get_fn, _raise_post,
        "okx.test", [(555, "BTC-USDT")], now_ms=1700003600000,
    )
    assert urls == ["https://okx.test/api/v5/market/candles?instId=BTC-USDT&bar=1m&limit=60"]
    assert stats.fetched == 1
    instrument, candles = series[555]
    assert instrument == "BTC-USDT"
    assert [c.ts_ms for c in candles] == [1700000000000, 1700000060000]


def test_fetch_deribit_candles_window_from_now_ms() -> None:
    urls: list[str] = []

    def get_fn(url: str) -> str | None:
        urls.append(url)
        return GOOD_DERIBIT

    claude_worker.fetchers.fetch_venue_candles(
        claude_worker.frames.VENUE_DERIBIT, _budget(9), get_fn, _raise_post,
        "d.test", [(556, "BTC-PERPETUAL")], now_ms=1700003600000,
    )
    assert urls == [
        "https://d.test/api/v2/public/get_tradingview_chart_data"
        "?instrument_name=BTC-PERPETUAL&start_timestamp=1700000000000"
        "&end_timestamp=1700003600000&resolution=1"
    ]


def test_fetch_hl_candles_post_body() -> None:
    posts: list[tuple[str, str]] = []

    def post_fn(url: str, body: str) -> str | None:
        posts.append((url, body))
        return GOOD_HL

    series, stats = claude_worker.fetchers.fetch_venue_candles(
        claude_worker.frames.VENUE_HYPERLIQUID, _budget(9), _raise_get, post_fn,
        "hl.test", [(HL_YES, "BTC")], now_ms=1700003600000,
    )
    assert stats.fetched == 1
    url, body = posts[0]
    assert url == "https://hl.test/info"
    assert json.loads(body) == {
        "type": "candleSnapshot",
        "req": {
            "coin": "BTC",
            "interval": "1m",
            "startTime": 1700000000000,
            "endTime": 1700003600000,
        },
    }
    assert series[HL_YES][0] == "BTC"


def test_fetch_candles_budget_exhausted_never_calls_get() -> None:
    series, stats = claude_worker.fetchers.fetch_venue_candles(
        claude_worker.frames.VENUE_OKX, _budget(0), _raise_get, _raise_post,
        "h", [(1, "A-B"), (2, "C-D")], now_ms=0,
    )
    assert series == {}
    assert stats == claude_worker.fetchers.FetchStats(2, 0, 2, 0, 0)


def test_fetch_candles_malformed_body_counted() -> None:
    series, stats = claude_worker.fetchers.fetch_venue_candles(
        claude_worker.frames.VENUE_DERIBIT, _budget(9), lambda url: "junk",
        _raise_post, "h", [(1, "X")], now_ms=0,
    )
    assert series == {}
    assert stats.malformed == 1


def test_fetch_candles_rejects_non_candle_venue() -> None:
    with pytest.raises(ValueError, match="not a candle venue"):
        claude_worker.fetchers.fetch_venue_candles(
            claude_worker.frames.VENUE_BINANCE, _budget(9), _raise_get,
            _raise_post, "h", [(7, "btcusdt")], now_ms=0,
        )


# ---- feature files ---------------------------------------------------------


def test_write_candle_file_beside_replay_features(tmp_path: pathlib.Path) -> None:
    candles = [
        claude_worker.fetchers.Candle(1700000000000, 1.0, 1.5, 0.5, 1.2, 10.0),
        claude_worker.fetchers.Candle(1700000060000, 2.0, 2.5, 1.5, 2.2, 11.0),
    ]
    path = claude_worker.fetchers.write_candle_file(
        tmp_path / "features", _RUN_NAME, HL_YES, 4, "BTC", candles
    )
    assert path == tmp_path / "features" / _RUN_NAME / f"{HL_YES}-ohlcv.json"
    obj = json.loads(path.read_text())
    assert obj["sym"] == HL_YES
    assert obj["venue"] == "hyperliquid"
    assert obj["instrument"] == "BTC"
    assert obj["interval"] == "1m"
    assert obj["candles"] == [
        [1700000000000, 1.0, 1.5, 0.5, 1.2, 10.0],
        [1700000060000, 2.0, 2.5, 1.5, 2.2, 11.0],
    ]


def test_write_market_meta_file(tmp_path: pathlib.Path) -> None:
    market = claude_worker.fetchers.GammaMarket(
        question="Q?", slug="q", token_ids=(TOK_A,), outcomes=("Yes", "No")
    )
    path = claude_worker.fetchers.write_market_meta_file(
        tmp_path / "features", _RUN_NAME, PM7, market
    )
    assert path == tmp_path / "features" / _RUN_NAME / f"{PM7}-meta.json"
    obj = json.loads(path.read_text())
    assert obj == {
        "sym": PM7,
        "venue": "polymarket",
        "question": "Q?",
        "slug": "q",
        "token_ids": [TOK_A],
        "outcomes": ["Yes", "No"],
    }


# ---- market map: bootstrap / refresh / conflicts / atomicity --------------


def test_default_names_engine_mirror() -> None:
    assert claude_worker.fetchers.default_names({7: 1}) == {"binance:btcusdt": 7}
    assert claude_worker.fetchers.default_names({7: 0}) == {}  # pm sym 7: no mirror
    assert claude_worker.fetchers.default_names({8: 1}) == {}  # non-default id


def test_gamma_names_question_and_slug() -> None:
    market = claude_worker.fetchers.GammaMarket("Q?", "q-slug", (TOK_A,), ())
    assert claude_worker.fetchers.gamma_names({PM7: market}) == {"Q?": PM7, "q-slug": PM7}


def test_refresh_bootstrap_complete_and_reader_loadable(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "market-map.json"
    refresh = claude_worker.fetchers.refresh_market_map(
        path, {}, (), {"binance:btcusdt": 7}, {7: 1, HL_YES: 4}
    )
    assert refresh.created
    assert refresh.added == {"binance:btcusdt": 7}
    assert refresh.conflicts == []
    assert refresh.unresolved == [(HL_YES, 4)]
    obj = json.loads(path.read_text())
    assert set(obj) == {"markets", "hip4_pairs"}  # complete shape
    # The write side must satisfy the UNTOUCHED reader contract.
    loaded = claude_worker.cli.load_market_map(path)
    assert loaded.markets == {"binance:btcusdt": 7}
    assert loaded.hip4_pairs == ()


def test_refresh_additive_preserves_operator_entries(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "market-map.json"
    operator = {"My Market": 42, "okx:BTC-USDT": (2 << 24) + 1}
    pairs = ((HL_YES, HL_NO),)
    path.write_text(
        json.dumps({"markets": operator, "hip4_pairs": [[HL_YES, HL_NO]]})
    )
    refresh = claude_worker.fetchers.refresh_market_map(
        path, operator, pairs, {"new-name": 42}, {42: 0}
    )
    loaded = claude_worker.cli.load_market_map(path)
    assert loaded.markets["My Market"] == 42  # operator entry verbatim
    assert loaded.markets["okx:BTC-USDT"] == (2 << 24) + 1
    assert loaded.markets["new-name"] == 42  # addition only
    assert loaded.hip4_pairs == ((HL_YES, HL_NO),)  # pairs preserved
    assert refresh.added == {"new-name": 42}
    assert not refresh.created


def test_refresh_conflict_reported_and_left_alone(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "market-map.json"
    refresh = claude_worker.fetchers.refresh_market_map(
        path, {"My Market": 42}, (), {"My Market": 43, "other": 43}, {42: 0, 43: 0}
    )
    assert len(refresh.conflicts) == 1
    assert "'My Market'" in refresh.conflicts[0]
    loaded = claude_worker.cli.load_market_map(path)
    assert loaded.markets["My Market"] == 42  # operator wins
    assert loaded.markets["other"] == 43


def test_refresh_never_fabricates_hip4_pairs(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "market-map.json"
    claude_worker.fetchers.refresh_market_map(
        path, {}, (), {}, {HL_YES: 4, HL_NO: 4}
    )
    assert json.loads(path.read_text())["hip4_pairs"] == []


def test_refresh_atomic_write_tmp_plus_replace(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "market-map.json"
    observed: list[tuple[str, str]] = []
    real_replace = os.replace

    def spy(src: typing.Any, dst: typing.Any) -> None:
        observed.append((str(src), str(dst)))
        real_replace(src, dst)

    monkeypatch.setattr(claude_worker.fetchers.os, "replace", spy)
    claude_worker.fetchers.refresh_market_map(path, {}, (), {"n": 1}, {1: 0})
    assert observed == [(str(path) + ".tmp", str(path))]
    assert pathlib.Path(observed[0][0]).parent == path.parent  # same dir
    assert not pathlib.Path(observed[0][0]).exists()  # tmp gone
    assert json.loads(path.read_text())["markets"] == {"n": 1}


# ---- run_secondary orchestration ------------------------------------------


def test_run_secondary_no_rest_is_real(tmp_path: pathlib.Path) -> None:
    """--no-rest skips ALL four consumers even with seeds present; the
    map is still owned (bootstrap runs, zero-REST resolutions only)."""
    map_path = tmp_path / "market-map.json"
    report = claude_worker.fetchers.run_secondary(
        universe={PM7: 0},
        markets={TOK_A: PM7, "hyperliquid:BTC": HL_YES},
        hip4_pairs=(),
        map_path=map_path,
        features_dir=tmp_path / "features",
        run_name=_RUN_NAME,
        no_rest=True,
        get_fn=None,
        post_fn=None,
        env={},
    )
    assert report.files == []
    assert report.lines[0] == "rest: skipped (--no-rest)"
    assert map_path.exists()
    assert any("market map bootstrapped" in line for line in report.lines)


def test_run_secondary_zero_targets_never_touches_client(
    tmp_path: pathlib.Path,
) -> None:
    """REST on, but no map names intersect the universe: the injected
    fns are never called (this pins the invariant that keeps the frozen
    fetch tests hermetic — no descriptors, no requests)."""
    map_path = tmp_path / "market-map.json"
    report = claude_worker.fetchers.run_secondary(
        universe={PM7: 0, HL_YES: 4, HL_NO: 4},
        markets={},
        hip4_pairs=(),
        map_path=map_path,
        features_dir=tmp_path / "features",
        run_name=_RUN_NAME,
        no_rest=False,
        get_fn=_raise_get,
        post_fn=_raise_post,
        env={},
    )
    assert report.files == []
    assert map_path.exists()
    assert json.loads(map_path.read_text())["markets"] == {}
    assert sum("unresolved sym" in line for line in report.lines) == 3


def test_run_secondary_full_pass(tmp_path: pathlib.Path) -> None:
    """Gamma + HL consumers fire from map seeds; resolutions land in the
    map; budget accounting (incl. skipped_total) is surfaced."""
    map_path = tmp_path / "market-map.json"
    calls: list[str] = []

    def get_fn(url: str) -> str | None:
        calls.append(url)
        return GOOD_GAMMA

    def post_fn(url: str, body: str) -> str | None:
        calls.append(url)
        return GOOD_HL

    report = claude_worker.fetchers.run_secondary(
        universe={PM7: 0, HL_YES: 4, HL_NO: 4},
        markets={TOK_A: PM7, "hyperliquid:BTC": HL_YES, "hyperliquid:zzz": HL_NO},
        hip4_pairs=((HL_YES, HL_NO),),
        map_path=map_path,
        features_dir=tmp_path / "features",
        run_name=_RUN_NAME,
        no_rest=False,
        get_fn=get_fn,
        post_fn=post_fn,
        env={"CLAUDE_WORKER_REST_BUDGET_PER_H": "1"},  # HL: 1 of 2 skipped
        now_ms=1700003600000,
    )
    assert calls[0].startswith("https://gamma-api.polymarket.com/markets?clob_token_ids=")
    assert calls[1] == "https://api.hyperliquid.xyz/info"
    assert len(calls) == 2  # second HL target budget-skipped
    names = sorted(p.name for p in report.files)
    assert names == [f"{HL_YES}-ohlcv.json", f"{PM7}-meta.json"]
    loaded = claude_worker.cli.load_market_map(map_path)
    assert loaded.markets["Xi Jinping out before 2027?"] == PM7
    assert loaded.markets["xi-out-2027"] == PM7
    assert loaded.markets[TOK_A] == PM7  # seed preserved
    assert loaded.hip4_pairs == ((HL_YES, HL_NO),)
    hl_line = next(line for line in report.lines if line.startswith("rest hyperliquid"))
    assert "budget_skipped=1" in hl_line
    assert "skipped_total=1" in hl_line
    assert any("market map bootstrapped" in line for line in report.lines)


# ---- fetch verb wiring (cli level) ----------------------------------------

_RUNNER = typer.testing.CliRunner()


@pytest.fixture
def fetch_env(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> pathlib.Path:
    replay = tmp_path / "replay"
    _mk_run(replay)
    monkeypatch.setenv("AI_INGRESS_SOCK", str(tmp_path / "ai.sock"))
    monkeypatch.setenv("AI_INGRESS_HMAC_KEY", tests.conftest.TEST_KEY.hex())
    monkeypatch.setenv("AI_RULESET_DIR", str(tmp_path / "rulesets"))
    monkeypatch.setenv("CLAUDE_WORKER_REPLAY_DIR", str(replay))
    monkeypatch.setenv("CLAUDE_WORKER_DB", str(tmp_path / "state.db"))
    monkeypatch.setenv("CLAUDE_WORKER_FEATURES_DIR", str(tmp_path / "features"))
    monkeypatch.setenv("CLAUDE_WORKER_MARKET_MAP", str(tmp_path / "market-map.json"))
    monkeypatch.setenv("RSS_FEEDS", "")
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    monkeypatch.delenv("CLAUDE_WORKER_REST_BUDGET_PER_H", raising=False)
    return tmp_path


def _forbid_client(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom() -> httpx.Client:
        raise AssertionError("_make_http_client must not be constructed")

    monkeypatch.setattr(claude_worker.cli, "_make_http_client", boom)


def test_cli_fetch_no_rest_never_constructs_client(
    fetch_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _forbid_client(monkeypatch)
    # Seeds present: without --no-rest these WOULD fetch.
    (fetch_env / "market-map.json").write_text(
        json.dumps({"markets": {TOK_A: PM7}, "hip4_pairs": []})
    )
    result = _RUNNER.invoke(claude_worker.cli.app, ["fetch", "--no-rest"])
    assert result.exit_code == 0, result.output
    loaded = claude_worker.cli.load_market_map(fetch_env / "market-map.json")
    assert loaded.markets == {TOK_A: PM7}  # refreshed additively, no REST


def test_cli_fetch_no_descriptors_zero_rest_and_bootstrap(
    fetch_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent map + no seeds: zero requests, client never constructed,
    map bootstrapped — the invariant the frozen fetch tests rely on."""
    _forbid_client(monkeypatch)
    result = _RUNNER.invoke(claude_worker.cli.app, ["fetch"])
    assert result.exit_code == 0, result.output
    obj = json.loads((fetch_env / "market-map.json").read_text())
    assert obj == {"markets": {}, "hip4_pairs": []}
    # Replay-derived feature files untouched by the secondary.
    out_dir = fetch_env / "features" / _RUN_NAME
    assert sorted(p.name for p in out_dir.glob("*.json")) == [
        f"{HL_YES}.json",
        f"{HL_NO}.json",
        "7.json",
    ]


def test_cli_fetch_rest_end_to_end_with_mock_transport(
    fetch_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (fetch_env / "market-map.json").write_text(
        json.dumps(
            {"markets": {TOK_A: PM7, "hyperliquid:BTC": HL_YES}, "hip4_pairs": []}
        )
    )
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(f"{request.method} {request.url}")
        if request.url.host == "gamma-api.polymarket.com":
            return httpx.Response(200, text=GOOD_GAMMA)
        if request.url.host == "api.hyperliquid.xyz":
            assert json.loads(request.content)["type"] == "candleSnapshot"
            return httpx.Response(200, text=GOOD_HL)
        raise AssertionError(f"unexpected host: {request.url}")

    monkeypatch.setattr(
        claude_worker.cli,
        "_make_http_client",
        lambda: httpx.Client(transport=httpx.MockTransport(handler)),
    )
    result = _RUNNER.invoke(claude_worker.cli.app, ["fetch"])
    assert result.exit_code == 0, result.output
    assert len(seen) == 2
    out_dir = fetch_env / "features" / _RUN_NAME
    assert (out_dir / f"{PM7}-meta.json").exists()
    assert (out_dir / f"{HL_YES}-ohlcv.json").exists()
    assert str(out_dir / f"{PM7}-meta.json") in result.output  # printed path
    loaded = claude_worker.cli.load_market_map(fetch_env / "market-map.json")
    assert loaded.markets["Xi Jinping out before 2027?"] == PM7
    assert loaded.markets["hyperliquid:BTC"] == HL_YES


def test_cli_fetch_rest_http_failure_is_counted_not_fatal(
    fetch_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (fetch_env / "market-map.json").write_text(
        json.dumps({"markets": {TOK_A: PM7}, "hip4_pairs": []})
    )

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(500, text="boom")

    monkeypatch.setattr(
        claude_worker.cli,
        "_make_http_client",
        lambda: httpx.Client(transport=httpx.MockTransport(handler)),
    )
    result = _RUNNER.invoke(claude_worker.cli.app, ["fetch"])
    assert result.exit_code == 0, result.output  # REST is best-effort
    assert (fetch_env / "market-map.json").exists()


def test_cli_fetch_malformed_map_is_exit_2_and_never_overwritten(
    fetch_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A half-readable operator map must never be 'repaired': the strict
    reader fails the verb BEFORE any write path runs."""
    _forbid_client(monkeypatch)
    map_path = fetch_env / "market-map.json"
    map_path.write_text('{"markets": {"x": "not-an-int"}}')
    before = map_path.read_text()
    result = _RUNNER.invoke(claude_worker.cli.app, ["fetch"])
    assert result.exit_code == 2
    assert map_path.read_text() == before
