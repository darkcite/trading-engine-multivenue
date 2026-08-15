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
import sqlite3
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


# ------------------------------------------------------- CLI surface (§11)


def _command_names() -> set[str]:
    names: set[str] = set()
    for info in claude_worker.cli.app.registered_commands:
        name = info.name
        if name is None:
            assert info.callback is not None
            name = info.callback.__name__.replace("_", "-")
        names.add(name)
    return names


def test_verb_surface_is_exactly_section_6() -> None:
    assert _command_names() == {
        "serve",
        "fetch",
        "backtest",
        "push",
        "positions",
        "stage-ruleset",
        "commit-ruleset",
    }


_OVERRIDE_TOKENS = ("--override", "--force", "--skip-gates", "--no-gates", "--bypass", "--unsafe")


def test_no_override_flag_parses_anywhere() -> None:
    """§11: the CLI-surface test — no override-shaped option exists on any
    verb's parsed surface, and an explicit --override does not parse."""
    for name in sorted(_command_names()):
        result = _invoke(name, "--help")
        assert result.exit_code == 0, (name, result.output)
        for token in _OVERRIDE_TOKENS:
            assert token not in result.output, (name, token)


def test_stage_ruleset_override_flag_is_rejected(cli_env: pathlib.Path) -> None:
    ruleset, _full = _mk_ruleset(cli_env)
    result = _invoke("stage-ruleset", "--ruleset", str(ruleset), "--report", "r.json", "--override")
    assert result.exit_code == 2, result.output


# ------------------------------------------- stage-ruleset / commit-ruleset


def _mk_passing_pair(tmp_path: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path, str]:
    """Ruleset + a gates-PASSED worker report written by the real
    backtest library (canned harness stdout)."""
    ruleset, full_hash = _mk_ruleset(tmp_path)
    outcome = claude_worker.backtest.run_backtest(
        ruleset, tmp_path, run_fn=lambda argv: _canned_report(full_hash)
    )
    assert outcome.all_passed
    return ruleset, outcome.report_path, full_hash


def _hash128_halves(full_hash: str) -> tuple[int, int]:
    h128 = bytes.fromhex(full_hash)[:16]
    px = int.from_bytes(h128[0:8], "little", signed=True)
    qty = int.from_bytes(h128[8:16], "little", signed=True)
    return px, qty


def test_stage_ruleset_happy_records_and_sends_hash128(
    uds_env: tests.conftest.FakeUdsServer, cli_env: pathlib.Path
) -> None:
    ruleset, report, full_hash = _mk_passing_pair(cli_env)
    result = _invoke("stage-ruleset", "--ruleset", str(ruleset), "--report", str(report))
    assert result.exit_code == 0, result.output
    _wait_for_frames(uds_env, 2)
    assert uds_env.cmd_field(0, "kind") == claude_worker.frames.KIND_HEARTBEAT
    assert uds_env.cmd_field(1, "kind") == claude_worker.frames.KIND_RULESET_STAGE
    px, qty = _hash128_halves(full_hash)
    assert uds_env.cmd_field(1, "px") == px
    assert uds_env.cmd_field(1, "qty") == qty
    assert uds_env.cmd_field(1, "strategy_id") == claude_worker.frames.STRATEGY_SLOT_VM
    assert uds_env.cmd_field(1, "venue") == claude_worker.frames.VENUE_AI
    assert uds_env.cmd_field(1, "sym") == claude_worker.frames.SYMBOL_ID_NONE
    assert f"staged {full_hash}" in result.output
    state = claude_worker.state.State(cli_env / "state.db")
    try:
        row = state.ruleset_row(full_hash)
    finally:
        state.close()
    assert row is not None
    assert row[3] is True  # gates_passed
    assert row[4] == "session"  # §8.7 attribution default for verbs
    assert row[5] is not None  # staged_ts
    assert row[6] is None  # not committed


def test_stage_refusals_are_exit_3_and_send_no_payload(
    uds_env: tests.conftest.FakeUdsServer, cli_env: pathlib.Path
) -> None:
    """§11 gate-refusal matrix: report missing / gates failed / schema
    wrong / hash mismatch. Every case: exit 3, heartbeat only (no Stage
    frame), no registry row."""
    ruleset, full_hash = _mk_ruleset(cli_env)

    # 1. report missing
    result = _invoke(
        "stage-ruleset", "--ruleset", str(ruleset), "--report", str(cli_env / "nope.json")
    )
    assert result.exit_code == 3, result.output

    # 2. gates failed (real library-written failing report)
    fail_outcome = claude_worker.backtest.run_backtest(
        ruleset, cli_env, run_fn=lambda argv: _canned_report(full_hash, pnl=-5.0)
    )
    assert not fail_outcome.all_passed
    result = _invoke(
        "stage-ruleset", "--ruleset", str(ruleset), "--report", str(fail_outcome.report_path)
    )
    assert result.exit_code == 3, result.output

    # 3. schema version wrong
    bad_schema = cli_env / "bad-schema.json"
    bad_schema.write_text(
        json.dumps({"schema_version": 99, "ruleset_hash": full_hash, "gates": {"all_passed": True}})
    )
    result = _invoke("stage-ruleset", "--ruleset", str(ruleset), "--report", str(bad_schema))
    assert result.exit_code == 3, result.output

    # 4. hash mismatch: valid report, then the ruleset file changes
    _ruleset2, report2, _full2 = _mk_passing_pair(cli_env)
    with (cli_env / "rs.json").open("a") as fh:
        fh.write("\n")
    result = _invoke("stage-ruleset", "--ruleset", str(ruleset), "--report", str(report2))
    assert result.exit_code == 3, result.output

    time.sleep(0.05)  # any stray payload frame would still be in flight
    kinds = {uds_env.cmd_field(i, "kind") for i in range(len(uds_env.frames))}
    assert kinds == {claude_worker.frames.KIND_HEARTBEAT}  # heartbeats only
    state = claude_worker.state.State(cli_env / "state.db")
    try:
        assert state.ruleset_row(full_hash) is None  # nothing was recorded
    finally:
        state.close()


def test_stage_ruleset_bad_by_is_exit_2(cli_env: pathlib.Path) -> None:
    ruleset, _full = _mk_ruleset(cli_env)
    result = _invoke(
        "stage-ruleset", "--ruleset", str(ruleset), "--report", "r.json", "--by", "operator"
    )
    assert result.exit_code == 2, result.output


def test_commit_ruleset_happy_by_hash_then_by_file(
    uds_env: tests.conftest.FakeUdsServer, cli_env: pathlib.Path
) -> None:
    ruleset, report, full_hash = _mk_passing_pair(cli_env)
    assert (
        _invoke("stage-ruleset", "--ruleset", str(ruleset), "--report", str(report)).exit_code == 0
    )
    result = _invoke("commit-ruleset", "--hash", full_hash)
    assert result.exit_code == 0, result.output
    _wait_for_frames(uds_env, 4)
    assert uds_env.cmd_field(3, "kind") == claude_worker.frames.KIND_RULESET_COMMIT
    px, qty = _hash128_halves(full_hash)
    assert uds_env.cmd_field(3, "px") == px
    assert uds_env.cmd_field(3, "qty") == qty
    state = claude_worker.state.State(cli_env / "state.db")
    try:
        row = state.ruleset_row(full_hash)
    finally:
        state.close()
    assert row is not None and row[6] is not None  # committed_ts stamped
    # --ruleset variant resolves the same hash from file bytes.
    result = _invoke("commit-ruleset", "--ruleset", str(ruleset))
    assert result.exit_code == 0, result.output


def test_commit_unstaged_hash_is_exit_3(
    uds_env: tests.conftest.FakeUdsServer, cli_env: pathlib.Path
) -> None:
    result = _invoke("commit-ruleset", "--hash", "ab" * 32)
    assert result.exit_code == 3, result.output
    time.sleep(0.05)
    kinds = {uds_env.cmd_field(i, "kind") for i in range(len(uds_env.frames))}
    assert kinds <= {claude_worker.frames.KIND_HEARTBEAT}  # no Commit frame


def test_commit_gates_failed_row_is_exit_3(
    uds_env: tests.conftest.FakeUdsServer, cli_env: pathlib.Path
) -> None:
    """A staged-but-failed row cannot exist via the library (stage refuses
    first) — simulate a corrupted registry with raw SQL and prove commit
    still refuses."""
    full_hash = "cd" * 32
    claude_worker.state.State(cli_env / "state.db").close()  # create schema
    conn = sqlite3.connect(str(cli_env / "state.db"))
    with conn:
        conn.execute(
            "INSERT INTO rulesets (hash, path, gates_passed, author_mode, staged_ts)"
            " VALUES (?, 'x.json', 0, 'session', 1)",
            (full_hash,),
        )
    conn.close()
    result = _invoke("commit-ruleset", "--hash", full_hash)
    assert result.exit_code == 3, result.output


def test_commit_arg_validation(cli_env: pathlib.Path) -> None:
    assert _invoke("commit-ruleset").exit_code == 2
    ruleset, _full = _mk_ruleset(cli_env)
    assert _invoke("commit-ruleset", "--hash", "ab" * 32, "--ruleset", str(ruleset)).exit_code == 2
    assert _invoke("commit-ruleset", "--hash", "zz").exit_code == 2


# ---------------------------------------------------------------- positions


def _mk_positions_run(
    tmp_path: pathlib.Path, name: str = _RUN_NAME, truncate_fills: int = 0
) -> pathlib.Path:
    run_dir = _mk_run(tmp_path, name=name)
    fills = _FIXTURES / "fills_v2.pmlr"
    target = run_dir / "engine-fills.pmlr"
    data = fills.read_bytes()
    target.write_bytes(data[: len(data) - truncate_fills] if truncate_fills else data)
    return run_dir


def test_positions_golden_fills_json(cli_env: pathlib.Path) -> None:
    """§11 positions row: golden fills fixture (Rust-writer bytes incl. a
    HIP-4 yes/no pair) -> known positions/exposure/P&L."""
    _mk_positions_run(cli_env)
    _write_market_map(cli_env, pairs=[[67119674, 67119675]])
    result = _invoke("positions", "--json")
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert data["fills_torn"] is False
    assert data["ticks_torn"] == []
    positions_by_sym = {p["sym"]: p for p in data["positions"]}
    assert sorted(positions_by_sym) == [7, 67119674, 67119675]
    p7 = positions_by_sym[7]
    assert p7["net_qty"] == pytest.approx(15.0)
    assert p7["avg_px"] == pytest.approx(0.486667, abs=1e-6)
    assert p7["mark_px"] == pytest.approx(0.51)
    assert p7["realized_usd"] == pytest.approx(0.50)
    assert p7["unrealized_usd"] == pytest.approx(0.35)
    assert p7["exposure_usd"] == pytest.approx(7.65)
    yes = positions_by_sym[67119674]
    assert yes["net_qty"] == pytest.approx(8.0)
    assert yes["unrealized_usd"] == pytest.approx(0.08)
    assert yes["exposure_usd"] == pytest.approx(4.88)
    assert len(data["hip4_pairs"]) == 1
    pair = data["hip4_pairs"][0]
    assert pair["yes_sym"] == 67119674 and pair["no_sym"] == 67119675
    assert pair["net_qty"] == pytest.approx(3.0)
    assert pair["flattened_qty"] == pytest.approx(5.0)
    assert pair["exposure_usd"] == pytest.approx(1.83)
    # HIP-4 netting: paired legs contribute the net-leg exposure only.
    assert data["total_exposure_usd"] == pytest.approx(7.65 + 1.83)


def test_positions_torn_fills_tail_tolerated(cli_env: pathlib.Path) -> None:
    """Reader stops cleanly mid-flush (§11): a truncated final fill (the
    no-leg buy) is ignored and flagged; the pair view degrades to a bare
    yes leg."""
    _mk_positions_run(cli_env, truncate_fills=10)
    _write_market_map(cli_env, pairs=[[67119674, 67119675]])
    result = _invoke("positions", "--json")
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert data["fills_torn"] is True
    positions_by_sym = {p["sym"]: p for p in data["positions"]}
    assert sorted(positions_by_sym) == [7, 67119674]  # no-leg fill lost
    pair = data["hip4_pairs"][0]
    assert pair["net_qty"] == pytest.approx(8.0)
    assert pair["flattened_qty"] == pytest.approx(0.0)
    assert pair["exposure_usd"] == pytest.approx(4.88)
    assert data["total_exposure_usd"] == pytest.approx(7.65 + 4.88)


def test_positions_latest_and_explicit_run_dir(cli_env: pathlib.Path) -> None:
    _mk_run(cli_env, name="run-1755216000000000000")  # older: ticks only
    newer = _mk_positions_run(cli_env, name="run-1755216000000000001")
    result = _invoke("positions", "--json")
    assert result.exit_code == 0, result.output
    assert json.loads(result.output)["run_dir"] == str(newer)
    result = _invoke("positions", "--run-dir", "latest", "--json")
    assert json.loads(result.output)["run_dir"] == str(newer)
    older = cli_env / "logs" / "run-1755216000000000000"
    result = _invoke("positions", "--run-dir", str(older), "--json")
    data = json.loads(result.output)
    assert data["run_dir"] == str(older)
    assert data["positions"] == []  # ticks-only run: no fills file
    result = _invoke("positions", "--run-dir", str(cli_env / "missing"))
    assert result.exit_code == 2


def test_positions_human_output(cli_env: pathlib.Path) -> None:
    _mk_positions_run(cli_env)
    _write_market_map(cli_env, pairs=[[67119674, 67119675]])
    result = _invoke("positions")
    assert result.exit_code == 0, result.output
    assert "sym 7  net 15.000000" in result.output
    assert "hip4 67119674/67119675  net 3.000000" in result.output
    assert "total exposure: 9.48 USD" in result.output


def test_positions_no_runs_is_exit_2(cli_env: pathlib.Path) -> None:
    assert _invoke("positions").exit_code == 2
