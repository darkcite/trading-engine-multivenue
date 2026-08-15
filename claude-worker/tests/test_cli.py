"""cli.py — operator verbs against the fake UDS server + canned reports
(design §6, §11).

Every test here runs with ``ANTHROPIC_API_KEY`` unset (the ``cli_env``
fixture deletes it): the §5.2 key-unset invariant is proven by the whole
module, plus the explicit tests at the bottom. No live engine, no live
feeds (httpx MockTransport), no SDK anywhere.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import json
import pathlib
import shutil
import time

import httpx
import pytest
import typer.testing

import claude_worker.backtest
import claude_worker.cli
import claude_worker.config
import claude_worker.daemon
import claude_worker.frames
import claude_worker.state
import tests.conftest

_FIXTURES = pathlib.Path(__file__).parent / "fixtures" / "pmlr"
_RUN_NAME = "run-1755216000000000000"
FEED_URL = "https://news.example/rss"

RSS_XML = """<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Example</title>
<item><guid>g1</guid><title>Bitcoin ETF approved</title>
<link>https://news.example/1</link>
<pubDate>Sat, 15 Aug 2026 10:00:00 GMT</pubDate>
<description>The SEC approved a spot ETF.</description></item>
<item><guid>g2</guid><title>Local sports roundup</title>
<link>https://news.example/2</link>
<pubDate>Sat, 15 Aug 2026 11:00:00 GMT</pubDate>
<description>The game went fine.</description></item>
</channel></rss>
"""

_RUNNER = typer.testing.CliRunner()


def _invoke(*args: str) -> typer.testing.Result:
    return _RUNNER.invoke(claude_worker.cli.app, list(args))


def _wait_for_frames(server: tests.conftest.FakeUdsServer, count: int) -> None:
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if len(server.frames) >= count:
            return
        time.sleep(0.01)
    raise AssertionError(
        f"fake server saw {len(server.frames)} frames, wanted {count}; errors={server.errors}"
    )


@pytest.fixture
def cli_env(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> collections.abc.Iterator[pathlib.Path]:
    """Full BaseConfig env pointing at test-local paths; the UDS socket
    path exists but nothing listens (transport tests override it). The
    Anthropic key is DELETED — every verb test proves the §5.2 split."""
    sock_dir = tests.conftest.short_sock_dir()
    replay = tmp_path / "logs"
    replay.mkdir()
    monkeypatch.setenv("AI_INGRESS_SOCK", str(sock_dir / "absent.sock"))
    monkeypatch.setenv("AI_INGRESS_HMAC_KEY", tests.conftest.TEST_KEY.hex())
    monkeypatch.setenv("AI_RULESET_DIR", str(tmp_path / "rulesets"))
    monkeypatch.setenv("CLAUDE_WORKER_REPLAY_DIR", str(replay))
    monkeypatch.setenv("CLAUDE_WORKER_DB", str(tmp_path / "state.db"))
    monkeypatch.setenv("CLAUDE_WORKER_FEATURES_DIR", str(tmp_path / "features"))
    monkeypatch.setenv("CLAUDE_WORKER_MARKET_MAP", str(tmp_path / "market-map.json"))
    monkeypatch.setenv("RSS_FEEDS", "")
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    yield tmp_path
    shutil.rmtree(sock_dir, ignore_errors=True)


@pytest.fixture
def uds_env(
    cli_env: pathlib.Path,
    fake_uds: tests.conftest.FakeUdsServer,
    monkeypatch: pytest.MonkeyPatch,
) -> tests.conftest.FakeUdsServer:
    """cli_env with the socket pointed at a RUNNING fake UDS server."""
    monkeypatch.setenv("AI_INGRESS_SOCK", str(fake_uds.sock_path))
    return fake_uds


def _write_market_map(
    tmp_path: pathlib.Path,
    markets: dict[str, int] | None = None,
    pairs: list[list[int]] | None = None,
) -> pathlib.Path:
    path = tmp_path / "market-map.json"
    payload: dict[str, object] = {}
    if markets is not None:
        payload["markets"] = markets
    if pairs is not None:
        payload["hip4_pairs"] = pairs
    path.write_text(json.dumps(payload))
    return path


def _mk_run(tmp_path: pathlib.Path, name: str = _RUN_NAME) -> pathlib.Path:
    run_dir = tmp_path / "logs" / name
    run_dir.mkdir(parents=True)
    shutil.copy(_FIXTURES / "ticks_v2.pmlr", run_dir / "polymarket-ticks.pmlr")
    return run_dir


# ---------------------------------------------------------------- push


def test_push_set_bias_happy(uds_env: tests.conftest.FakeUdsServer) -> None:
    result = _invoke(
        "push",
        "--kind",
        "set-bias",
        "--sym",
        "7",
        "--px",
        "-0.02",
        "--ttl-s",
        "30",
        "--expire-on-silence",
    )
    assert result.exit_code == 0, result.output
    _wait_for_frames(uds_env, 2)
    assert uds_env.errors == []
    # Implicit heartbeat first (§5.4), then the payload.
    assert uds_env.cmd_field(0, "kind") == claude_worker.frames.KIND_HEARTBEAT
    assert uds_env.cmd_field(1, "kind") == claude_worker.frames.KIND_SET_BIAS
    assert uds_env.cmd_field(1, "sym") == 7
    assert uds_env.cmd_field(1, "px") == -20_000  # -0.02 x 1e6, signed
    assert uds_env.cmd_field(1, "qty") == 0
    assert uds_env.cmd_field(1, "ttl_ns") == 30_000_000_000
    assert uds_env.cmd_field(1, "venue") == claude_worker.frames.VENUE_AI
    assert uds_env.cmd_field(1, "strategy_id") == claude_worker.frames.STRATEGY_SLOT_NONE
    assert uds_env.cmd_field(1, "side") == claude_worker.frames.SIDE_NONE
    assert uds_env.cmd_field(1, "flags") == claude_worker.frames.FLAG_EXPIRE_ON_SILENCE
    assert "sent kind=set-bias seq=" in result.output


def test_push_heartbeat_sends_exactly_one_frame(uds_env: tests.conftest.FakeUdsServer) -> None:
    result = _invoke("push", "--kind", "heartbeat")
    assert result.exit_code == 0, result.output
    _wait_for_frames(uds_env, 1)
    time.sleep(0.05)  # grace: a stray second frame would still be in flight
    assert len(uds_env.frames) == 1
    assert uds_env.cmd_field(0, "kind") == claude_worker.frames.KIND_HEARTBEAT


def test_push_enable_and_symbol_resolution(
    uds_env: tests.conftest.FakeUdsServer, cli_env: pathlib.Path
) -> None:
    _write_market_map(cli_env, markets={"fed-cut-2026": 12})
    result = _invoke("push", "--kind", "enable", "--strategy", "4")
    assert result.exit_code == 0, result.output
    result = _invoke(
        "push",
        "--kind",
        "set-fair-value",
        "--symbol",
        "fed-cut-2026",
        "--px",
        "0.61",
        "--ttl-s",
        "60",
    )
    assert result.exit_code == 0, result.output
    _wait_for_frames(uds_env, 4)
    assert uds_env.cmd_field(1, "kind") == claude_worker.frames.KIND_ENABLE_STRATEGY
    assert uds_env.cmd_field(1, "strategy_id") == 4
    assert uds_env.cmd_field(1, "sym") == claude_worker.frames.SYMBOL_ID_NONE
    assert uds_env.cmd_field(3, "kind") == claude_worker.frames.KIND_SET_FAIR_VALUE
    assert uds_env.cmd_field(3, "sym") == 12  # resolved via the market map
    assert uds_env.cmd_field(3, "px") == 610_000
    assert uds_env.cmd_field(3, "flags") == 0


def test_push_order_intent_pins_ai_exec_slot(uds_env: tests.conftest.FakeUdsServer) -> None:
    result = _invoke(
        "push",
        "--kind",
        "order-intent",
        "--sym",
        "3",
        "--venue",
        "polymarket",
        "--side",
        "bid",
        "--px",
        "0.55",
        "--qty",
        "10",
        "--ttl-s",
        "5",
    )
    assert result.exit_code == 0, result.output
    _wait_for_frames(uds_env, 2)
    assert uds_env.cmd_field(1, "kind") == claude_worker.frames.KIND_ORDER_INTENT
    assert uds_env.cmd_field(1, "venue") == claude_worker.frames.VENUE_POLYMARKET
    assert uds_env.cmd_field(1, "side") == claude_worker.frames.SIDE_BID
    assert uds_env.cmd_field(1, "px") == 550_000
    assert uds_env.cmd_field(1, "qty") == 10_000_000
    # §3: strategy_id is pinned to the ai-exec slot, not operator input.
    assert uds_env.cmd_field(1, "strategy_id") == claude_worker.frames.STRATEGY_SLOT_AI_EXEC


def test_push_set_param_and_halt(uds_env: tests.conftest.FakeUdsServer) -> None:
    result = _invoke(
        "push", "--kind", "set-param", "--strategy", "1", "--param-id", "3", "--px", "0.5"
    )
    assert result.exit_code == 0, result.output
    result = _invoke("push", "--kind", "halt")
    assert result.exit_code == 0, result.output
    _wait_for_frames(uds_env, 4)
    assert uds_env.cmd_field(1, "kind") == claude_worker.frames.KIND_SET_PARAM
    assert uds_env.cmd_field(1, "strategy_id") == 1
    assert uds_env.cmd_field(1, "param_id") == 3
    assert uds_env.cmd_field(1, "px") == 500_000
    assert uds_env.cmd_field(1, "ttl_ns") == 0
    assert uds_env.cmd_field(3, "kind") == claude_worker.frames.KIND_HALT_REQUEST
    assert uds_env.cmd_field(3, "sym") == claude_worker.frames.SYMBOL_ID_NONE
    assert uds_env.cmd_field(3, "strategy_id") == claude_worker.frames.STRATEGY_SLOT_NONE


_BAD_PUSH: list[tuple[list[str], str]] = [
    (["--kind", "set-bias", "--sym", "7", "--px", "0.01"], "missing-ttl"),
    (["--kind", "set-bias", "--px", "0.01", "--ttl-s", "30"], "missing-sym"),
    (["--kind", "enable"], "missing-strategy"),
    (["--kind", "enable", "--strategy", "1", "--px", "1"], "forbidden-px"),
    (["--kind", "enable", "--strategy", "1", "--expire-on-silence"], "forbidden-flag"),
    (["--kind", "halt", "--sym", "1"], "forbidden-sym"),
    (["--kind", "heartbeat", "--ttl-s", "5"], "forbidden-ttl"),
    (["--kind", "resume"], "unknown-kind"),
    (
        ["--kind", "set-bias", "--sym", "1", "--symbol", "x", "--px", "0.01", "--ttl-s", "5"],
        "both-sym-forms",
    ),
    (
        ["--kind", "set-bias", "--symbol", "unmapped", "--px", "0.01", "--ttl-s", "5"],
        "unmapped-symbol",
    ),
    (
        ["--kind", "set-fair-value", "--sym", "1", "--px", "-0.5", "--ttl-s", "5"],
        "negative-fair-value",
    ),
    (
        [
            "--kind",
            "order-intent",
            "--sym",
            "1",
            "--venue",
            "ai",
            "--side",
            "bid",
            "--px",
            "0.5",
            "--qty",
            "1",
            "--ttl-s",
            "5",
        ],
        "venue-ai-intent",
    ),
    (
        [
            "--kind",
            "order-intent",
            "--sym",
            "1",
            "--venue",
            "polymarket",
            "--side",
            "mid",
            "--px",
            "0.5",
            "--qty",
            "1",
            "--ttl-s",
            "5",
        ],
        "bad-side",
    ),
    (
        [
            "--kind",
            "order-intent",
            "--sym",
            "1",
            "--venue",
            "polymarket",
            "--side",
            "bid",
            "--px",
            "0.5",
            "--qty",
            "1",
            "--ttl-s",
            "5",
            "--strategy",
            "4",
        ],
        "explicit-strategy-on-intent",
    ),
    (
        [
            "--kind",
            "order-intent",
            "--sym",
            "1",
            "--venue",
            "polymarket",
            "--side",
            "bid",
            "--px",
            "0.5",
            "--qty",
            "-1",
            "--ttl-s",
            "5",
        ],
        "negative-qty",
    ),
    (["--kind", "set-bias", "--sym", "1", "--px", "0.01", "--ttl-s", "0"], "zero-ttl"),
    (["--kind", "enable", "--strategy", "8"], "slot-out-of-range"),
    (["--kind", "set-param", "--strategy", "1", "--px", "1"], "missing-param-id"),
    (["--kind", "set-bias", "--sym", "4294967295", "--px", "0.01", "--ttl-s", "5"], "sym-sentinel"),
]


@pytest.mark.parametrize(("args", "label"), _BAD_PUSH, ids=[label for _, label in _BAD_PUSH])
def test_push_validation_refuses_before_any_socket_work(
    cli_env: pathlib.Path, args: list[str], label: str
) -> None:
    # cli_env's socket path has NO listener: reaching the socket would be
    # exit 4, so exit 2 here proves validation precedes transport.
    result = _invoke("push", *args)
    assert result.exit_code == 2, (label, result.output)


def test_push_validation_burns_no_seq(cli_env: pathlib.Path) -> None:
    for args, _label in _BAD_PUSH[:3]:
        _invoke("push", *args)
    state = claude_worker.state.State(cli_env / "state.db")
    try:
        assert state.peek_seq() == 1  # nothing allocated
    finally:
        state.close()


def test_push_engine_down_is_exit_4(cli_env: pathlib.Path) -> None:
    result = _invoke("push", "--kind", "heartbeat")
    assert result.exit_code == 4, result.output


# ---------------------------------------------------------------- fetch


def test_fetch_writes_feature_files_and_prints_paths(cli_env: pathlib.Path) -> None:
    _mk_run(cli_env)
    result = _invoke("fetch")
    assert result.exit_code == 0, result.output
    out_dir = cli_env / "features" / _RUN_NAME
    written = sorted(p.name for p in out_dir.glob("*.json"))
    assert written == ["67119674.json", "67119675.json", "7.json"]
    for name in written:
        assert str(out_dir / name) in result.output


def test_fetch_symbols_filter(cli_env: pathlib.Path) -> None:
    _mk_run(cli_env)
    result = _invoke("fetch", "--symbols", "7")
    assert result.exit_code == 0, result.output
    out_dir = cli_env / "features" / _RUN_NAME
    assert sorted(p.name for p in out_dir.glob("*.json")) == ["7.json"]


def test_fetch_replay_dir_override_and_no_runs(
    cli_env: pathlib.Path, tmp_path: pathlib.Path
) -> None:
    other = tmp_path / "other-logs"
    other.mkdir()
    result = _invoke("fetch", "--replay-dir", str(other))
    assert result.exit_code == 2  # no run-* dirs
    result = _invoke("fetch")
    assert result.exit_code == 2  # default replay dir is empty too


def test_fetch_news_dedupes_and_writes_ndjson(
    cli_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _mk_run(cli_env)
    monkeypatch.setenv("RSS_FEEDS", FEED_URL)

    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text=RSS_XML)

    def fake_client() -> httpx.Client:
        return httpx.Client(transport=httpx.MockTransport(handler))

    monkeypatch.setattr(claude_worker.cli, "_make_http_client", fake_client)
    # Pre-seeded dedupe row (§11): g1 has been seen before.
    state = claude_worker.state.State(cli_env / "state.db")
    state.mark_seen(FEED_URL, "g1", 0)
    state.close()

    result = _invoke("fetch", "--news")
    assert result.exit_code == 0, result.output
    ndjson_files = list((cli_env / "features" / "news").glob("items-*.ndjson"))
    assert len(ndjson_files) == 1
    lines = ndjson_files[0].read_text().splitlines()
    assert len(lines) == 1
    item = json.loads(lines[0])
    assert item == {
        "id": "g2",
        "feed": FEED_URL,
        "ts": item["ts"],
        "title": "Local sports roundup",
        "link": "https://news.example/2",
        "text": "The game went fine.",
    }
    assert str(ndjson_files[0]) in result.output


# ---------------------------------------------------------------- backtest


def _canned_report(full_hash: str, pnl: float = 12.5) -> str:
    return json.dumps(
        {
            "schema_version": claude_worker.backtest.REPORT_SCHEMA_VERSION,
            "ruleset_hash": full_hash,
            "split": "70/30",
            "oos": {
                "net_pnl_usd": pnl,
                "trades": 80,
                "trading_days": 3,
                "max_drawdown_usd": 50.0,
            },
            "bounds": {
                "max_order_notional_usd": 50.0,
                "max_symbol_notional_usd": 100.0,
                "max_total_notional_usd": 500.0,
            },
        }
    )


def _mk_ruleset(tmp_path: pathlib.Path) -> tuple[pathlib.Path, str]:
    path = tmp_path / "rs.json"
    path.write_text('{"rows": []}')
    full_hash, _hash128 = claude_worker.backtest.ruleset_hashes(path)
    return path, full_hash


def test_backtest_pass_is_exit_0_with_report(
    cli_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ruleset, full_hash = _mk_ruleset(cli_env)
    monkeypatch.setattr(
        claude_worker.backtest, "default_run_fn", lambda argv: _canned_report(full_hash)
    )
    result = _invoke("backtest", "--ruleset", str(ruleset))
    assert result.exit_code == 0, result.output
    report = claude_worker.backtest.report_path_for(ruleset)
    assert report.is_file()
    assert str(report) in result.output
    assert "-> PASS" in result.output


def test_backtest_gate_fail_is_exit_3_report_still_written(
    cli_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ruleset, full_hash = _mk_ruleset(cli_env)
    monkeypatch.setattr(
        claude_worker.backtest,
        "default_run_fn",
        lambda argv: _canned_report(full_hash, pnl=-5.0),
    )
    result = _invoke("backtest", "--ruleset", str(ruleset))
    assert result.exit_code == 3, result.output
    report = claude_worker.backtest.report_path_for(ruleset)
    assert report.is_file()  # §6: report written on fail too
    assert "-> FAIL" in result.output


def test_backtest_untrusted_harness_is_exit_2_no_report(
    cli_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ruleset, _full_hash = _mk_ruleset(cli_env)
    monkeypatch.setattr(claude_worker.backtest, "default_run_fn", lambda argv: "not json")
    result = _invoke("backtest", "--ruleset", str(ruleset))
    assert result.exit_code == 2, result.output
    assert not claude_worker.backtest.report_path_for(ruleset).exists()


def test_backtest_missing_ruleset_is_exit_2(cli_env: pathlib.Path) -> None:
    result = _invoke("backtest", "--ruleset", str(cli_env / "absent.json"))
    assert result.exit_code == 2, result.output


# ---------------------------------------------------------------- serve


def test_serve_wires_serve_config_and_market_map(
    cli_env: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _write_market_map(cli_env, markets={"btc-daily": 7})
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-test-cli")
    calls: list[tuple[claude_worker.config.ServeConfig, dict[str, int]]] = []

    def stub(cfg: claude_worker.config.ServeConfig, **kwargs: object) -> int:
        calls.append((cfg, typing_symbol_map(kwargs)))
        return 0

    def typing_symbol_map(kwargs: dict[str, object]) -> dict[str, int]:
        value = kwargs.get("symbol_map")
        assert isinstance(value, dict)
        return value

    monkeypatch.setattr(claude_worker.daemon, "serve", stub)
    result = _invoke("serve")
    assert result.exit_code == 0, result.output
    assert len(calls) == 1
    cfg, symbol_map = calls[0]
    assert isinstance(cfg, claude_worker.config.ServeConfig)
    assert cfg.anthropic_api_key == "sk-ant-test-cli"
    assert symbol_map == {"btc-daily": 7}


def test_serve_requires_api_key(cli_env: pathlib.Path) -> None:
    result = _invoke("serve")
    assert result.exit_code == 2, result.output  # ServeConfig validation


# ------------------------------------------------- key-unset invariant


def test_verbs_succeed_with_anthropic_key_unset(
    uds_env: tests.conftest.FakeUdsServer, cli_env: pathlib.Path
) -> None:
    """§5.2 key-unset invariant, explicitly: BaseConfig-only verbs — data
    AND frame-sending — succeed with ``ANTHROPIC_API_KEY`` absent (the
    fixture deleted it). ``serve`` alone demands the key
    (test_serve_requires_api_key)."""
    _mk_run(cli_env)
    assert _invoke("fetch").exit_code == 0
    assert _invoke("push", "--kind", "heartbeat").exit_code == 0


# ---------------------------------------------------------------- market map


def test_market_map_missing_file_is_empty(tmp_path: pathlib.Path) -> None:
    mm = claude_worker.cli.load_market_map(tmp_path / "absent.json")
    assert mm.markets == {}
    assert mm.hip4_pairs == ()


def test_market_map_full_parse(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "mm.json"
    path.write_text(json.dumps({"markets": {"a": 1, "b": 2}, "hip4_pairs": [[3, 4], [5, 6]]}))
    mm = claude_worker.cli.load_market_map(path)
    assert mm.markets == {"a": 1, "b": 2}
    assert mm.hip4_pairs == ((3, 4), (5, 6))


@pytest.mark.parametrize(
    "payload",
    [
        "not json",
        '["array"]',
        '{"unknown_key": {}}',
        '{"markets": {"a": true}}',
        '{"markets": {"a": -1}}',
        '{"markets": {"a": 4294967295}}',
        '{"markets": {"": 1}}',
        '{"hip4_pairs": [[1]]}',
        '{"hip4_pairs": [[1, 1]]}',
        '{"hip4_pairs": {"yes": 1}}',
    ],
)
def test_market_map_malformed_raises(tmp_path: pathlib.Path, payload: str) -> None:
    path = tmp_path / "mm.json"
    path.write_text(payload)
    with pytest.raises(ValueError):
        claude_worker.cli.load_market_map(path)
