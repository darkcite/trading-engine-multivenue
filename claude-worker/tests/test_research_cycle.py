"""daemon.ResearchCycle — the §7.4 cycle state machine, §7.5 budget
guards, §7.6 background-thread seam, and the §8.1/§8.2 promotion path
against the FakeUdsServer. No live SDK, no real binary, no live sockets
beyond the fixture (house rules).

Convention: full ``import x`` only. No ``from x import y``.
"""

import concurrent.futures
import collections.abc
import json
import pathlib
import threading
import time
import types
import typing

import anthropic
import pytest

import claude_worker.backtest
import claude_worker.config
import claude_worker.daemon
import claude_worker.frames
import claude_worker.llm
import claude_worker.state
import claude_worker.strategist
import claude_worker.uds
import tests.conftest

_DAY_NS = 86_400_000_000_000
_FIXED_WALL_NS = 3 * _DAY_NS + 43_200_000_000_000  # day 3, 12:00 UTC

_PROPOSAL = json.dumps(
    {
        "thesis": "fade the lagged PM quote",
        "rows": [
            {
                "name": "auto-buy-low",
                "family": "crypto",
                "trigger": {"type": "level_breach", "level": 0.42},
                "sym": 42,
                "side": "bid",
                "edge_bps": 80,
                "horizon_ms": 1500,
                "max_risk_usd": 50.0,
            }
        ],
    }
)
_REVISION = json.dumps(
    {
        "thesis": "wider level after gate fail",
        "rows": [
            {
                "name": "auto-buy-lower",
                "family": "crypto",
                "trigger": {"type": "level_breach", "level": 0.38},
                "sym": 42,
                "side": "bid",
                "edge_bps": 90,
                "horizon_ms": 1500,
                "max_risk_usd": 40.0,
            }
        ],
    }
)


def _mk_cfg(tmp_path: pathlib.Path, sock: pathlib.Path | None = None) -> claude_worker.config.BaseConfig:
    (tmp_path / "replay" / "run-1").mkdir(parents=True, exist_ok=True)
    return claude_worker.config.BaseConfig(
        ai_ingress_sock=tmp_path / "ai.sock" if sock is None else sock,
        ai_ingress_hmac_key=tests.conftest.TEST_KEY,
        ai_ruleset_dir=tmp_path / "rulesets",
        replay_dir=tmp_path / "replay",
        db_path=tmp_path / "worker" / "state.db",
        features_dir=tmp_path / "features",
        market_map_path=tmp_path / "market-map.json",
        rss_feeds=(),
    )


class _Clock:
    def __init__(self) -> None:
        self.now_ns: int = 0

    def __call__(self) -> int:
        return self.now_ns


class _CompleteFake:
    """Strategist completion seam double: canned responses, records the
    static block, prompt, and the CALLING THREAD (the §7.6 assertion)."""

    def __init__(self, responses: list[str]) -> None:
        self._responses = responses
        self.calls: list[tuple[list[dict[str, object]], str, int]] = []

    def __call__(
        self, system: list[dict[str, object]], prompt: str
    ) -> claude_worker.llm.Completion:
        self.calls.append((system, prompt, threading.get_ident()))
        text = self._responses[min(len(self.calls) - 1, len(self._responses) - 1)]
        return claude_worker.llm.Completion(text, 111, 42, 0, 0)


def _fake_run_backtest(
    passing: bool,
) -> typing.Callable[..., claude_worker.backtest.BacktestOutcome]:
    def run(
        ruleset_path: pathlib.Path,
        replay_dir: pathlib.Path,
        split: str = "70/30",
    ) -> claude_worker.backtest.BacktestOutcome:
        assert split == "70/30", "the frozen worker default"
        assert replay_dir.name == "replay"
        full_hash, _ = claude_worker.backtest.ruleset_hashes(ruleset_path)
        harness = claude_worker.backtest.HarnessReport(
            ruleset_hash=full_hash,
            split=split,
            oos_net_pnl_usd=5.0 if passing else -1.0,
            oos_trades=60,
            oos_trading_days=3,
            oos_max_drawdown_usd=20.0,
            max_order_notional_usd=50.0,
            max_symbol_notional_usd=96.8,
            max_total_notional_usd=96.8,
        )
        thresholds = claude_worker.backtest.GateThresholds()
        gates = claude_worker.backtest.evaluate_gates(harness, thresholds)
        report_path = claude_worker.backtest.write_report(
            ruleset_path, full_hash, harness, gates, thresholds
        )
        return claude_worker.backtest.BacktestOutcome(
            all_passed=gates.all_passed, report_path=report_path, gates=gates, harness=harness
        )

    return run


def _mk_cycle(  # noqa: PLR0913 — test composition root
    cfg: claude_worker.config.BaseConfig,
    state: claude_worker.state.State,
    executor: concurrent.futures.Executor,
    complete: _CompleteFake,
    clock: _Clock,
    *,
    passing: bool = True,
    env: dict[str, str] | None = None,
    fetch_fn: typing.Callable[[], bool] | None = None,
    wall_ns: typing.Callable[[], int] | None = None,
) -> claude_worker.daemon.ResearchCycle:
    return claude_worker.daemon.ResearchCycle(
        state,
        cfg,
        {"btc-daily": 42},
        complete,
        executor,
        fetch_fn=(lambda: True) if fetch_fn is None else fetch_fn,
        run_backtest_fn=_fake_run_backtest(passing),
        env={"CLAUDE_WORKER_STRATEGIST_INTERVAL_S": "1"} if env is None else env,
        clock_ns=clock,
        wall_ns=(lambda: _FIXED_WALL_NS) if wall_ns is None else wall_ns,
    )


def _drive(
    cycle: claude_worker.daemon.ResearchCycle,
    clock: _Clock,
    client: claude_worker.uds.UdsClient,
    done: typing.Callable[[], bool],
    timeout_s: float = 10.0,
) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        cycle.maybe_run(clock(), client)
        if done():
            return
        time.sleep(0.005)
    raise AssertionError("research cycle did not reach the expected condition")


@pytest.fixture
def executor() -> collections.abc.Iterator[concurrent.futures.ThreadPoolExecutor]:
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=1, thread_name_prefix="cw-test")
    yield pool
    pool.shutdown(wait=True, cancel_futures=True)


def _disconnected_client(
    cfg: claude_worker.config.BaseConfig, state: claude_worker.state.State
) -> claude_worker.uds.UdsClient:
    return claude_worker.uds.UdsClient(cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state)


# ---- cadence + capture skip ----------------------------------------------


def test_first_cycle_due_one_interval_after_start(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL])
    cycle = _mk_cycle(cfg, state, executor, complete, clock)
    client = _disconnected_client(cfg, state)
    # Not due before start + interval (the frozen-serve hermeticity rule).
    for _ in range(5):
        cycle.maybe_run(clock(), client)
    assert cycle.stats.cycles_started == 0
    clock.now_ns = 2_000_000_000  # past the 1 s test interval
    cycle.maybe_run(clock(), client)
    assert cycle.stats.cycles_started == 1
    state.close()


def test_capture_skip_when_no_runs_and_when_stale(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    (cfg.replay_dir / "run-1").rmdir()  # no capture at all
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake(["garbage"])
    cycle = _mk_cycle(cfg, state, executor, complete, clock)
    client = _disconnected_client(cfg, state)

    clock.now_ns = 2_000_000_000
    cycle.maybe_run(clock(), client)
    assert cycle.stats.skips_no_capture == 1
    skips = state.events(kind=claude_worker.strategist.EVENT_CAPTURE_SKIP)
    assert len(skips) == 1 and json.loads(skips[0][3]) == {"latest": None}

    # A run appears: the next due cycle consumes it (malformed output
    # ends it quickly)...
    (cfg.replay_dir / "run-7").mkdir()
    clock.now_ns = 4_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.candidates_rejected == 1)
    assert len(complete.calls) == 1
    # ...and the SAME run does not fire a second cycle.
    clock.now_ns = 8_000_000_000
    cycle.maybe_run(clock(), client)
    assert cycle.stats.skips_no_capture == 2
    assert json.loads(state.events(kind=claude_worker.strategist.EVENT_CAPTURE_SKIP)[1][3]) == {
        "latest": "run-7"
    }
    assert len(complete.calls) == 1, "no fresh capture => no call (§7.4)"
    state.close()


# ---- §7.5 budget guards --------------------------------------------------


def test_budget_cap_zero_is_a_kill_switch(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL])
    cycle = _mk_cycle(
        cfg, state, executor, complete, clock,
        env={
            "CLAUDE_WORKER_STRATEGIST_INTERVAL_S": "1",
            "CLAUDE_WORKER_STRATEGIST_DAILY_CAP": "0",
        },
    )
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.skips_budget == 1)
    assert complete.calls == []
    skip = state.events(kind=claude_worker.strategist.EVENT_BUDGET_SKIP)
    assert len(skip) == 1
    detail = json.loads(skip[0][3])
    assert detail == {"calls_today": 0, "daily_cap": 0, "purpose": "proposal"}
    state.close()


def test_budget_counts_only_todays_ledger_rows(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    kind = claude_worker.strategist.EVENT_STRATEGIST_CALL
    # 12 calls burned YESTERDAY: today's ceiling is untouched.
    for _ in range(12):
        state.record_event(kind, "{}", ts_ns=_FIXED_WALL_NS - _DAY_NS)
    clock = _Clock()
    complete = _CompleteFake(["not json"])
    cycle = _mk_cycle(cfg, state, executor, complete, clock)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.candidates_rejected == 1)
    assert len(complete.calls) == 1, "yesterday's ledger rows must not count"

    # 12 burned TODAY: the next fresh cycle budget-skips.
    for _ in range(12):
        state.record_event(kind, "{}", ts_ns=_FIXED_WALL_NS - 1_000_000)
    (cfg.replay_dir / "run-2").mkdir()
    clock.now_ns = 4_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.skips_budget == 1)
    assert len(complete.calls) == 1
    state.close()


# ---- dedupe (§7.4) -------------------------------------------------------


def test_restart_identical_inputs_is_a_dedupe_hit(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete_a = _CompleteFake(["zzz-not-json"])
    cycle_a = _mk_cycle(cfg, state, executor, complete_a, clock)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle_a, clock, client, lambda: cycle_a.stats.candidates_rejected == 1)
    assert len(complete_a.calls) == 1
    assert cycle_a.stats.calls == 1

    # A fresh instance (serve restart) over the SAME capture + files:
    # identical prompt => replayed response, zero API cost, no new
    # ledger row — but the flow still parses/archives.
    complete_b = _CompleteFake(["never-used"])
    cycle_b = _mk_cycle(cfg, state, executor, complete_b, clock)
    clock.now_ns = 4_000_000_000
    _drive(cycle_b, clock, client, lambda: cycle_b.stats.candidates_rejected == 1)
    assert complete_b.calls == []
    assert cycle_b.stats.dedupe_hits == 1 and cycle_b.stats.calls == 0
    assert len(state.events(kind=claude_worker.strategist.EVENT_STRATEGIST_CALL)) == 1
    state.close()


# ---- failure modes -------------------------------------------------------


def test_call_failure_is_an_event_not_a_crash(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()

    def boom(_system: list[dict[str, object]], _prompt: str) -> claude_worker.llm.Completion:
        raise RuntimeError("api down")

    cycle = claude_worker.daemon.ResearchCycle(
        state, cfg, {}, boom, executor,
        fetch_fn=lambda: True,
        run_backtest_fn=_fake_run_backtest(True),
        env={"CLAUDE_WORKER_STRATEGIST_INTERVAL_S": "1"},
        clock_ns=clock,
        wall_ns=lambda: _FIXED_WALL_NS,
    )
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.call_failures == 1)
    failed = state.events(kind=claude_worker.strategist.EVENT_CALL_FAILED)
    assert len(failed) == 1 and "api down" in json.loads(failed[0][3])["error"]
    assert state.events(kind=claude_worker.state.EVENT_FRAME_SENT) == []
    state.close()


def test_backtest_error_ends_cycle_without_revision(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL, _REVISION])

    def untrusted(
        _ruleset: pathlib.Path, _replay: pathlib.Path, split: str = "70/30"
    ) -> claude_worker.backtest.BacktestOutcome:
        del split
        raise claude_worker.backtest.BacktestError("validator reject")

    cycle = claude_worker.daemon.ResearchCycle(
        state, cfg, {"btc-daily": 42}, complete, executor,
        fetch_fn=lambda: True,
        run_backtest_fn=untrusted,
        env={"CLAUDE_WORKER_STRATEGIST_INTERVAL_S": "1"},
        clock_ns=clock,
        wall_ns=lambda: _FIXED_WALL_NS,
    )
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.backtest_errors == 1)
    assert len(complete.calls) == 1, "an untrusted report earns NO revision call"
    rejected = state.events(kind=claude_worker.strategist.EVENT_CANDIDATE_REJECTED)
    assert len(rejected) == 1 and json.loads(rejected[0][3])["reason"] == "backtest_error"
    assert not cfg.ai_ruleset_dir.exists(), "no install"
    assert state.events(kind=claude_worker.state.EVENT_FRAME_SENT) == []
    state.close()


def test_fetch_failure_is_counted_and_cycle_proceeds(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake(["not json"])
    fetch_threads: list[int] = []

    def failing_fetch() -> bool:
        fetch_threads.append(threading.get_ident())
        return False

    cycle = _mk_cycle(cfg, state, executor, complete, clock, fetch_fn=failing_fetch)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.candidates_rejected == 1)
    assert cycle.stats.fetch_failures == 1
    assert len(complete.calls) == 1, "fetch is best-effort enrichment"
    assert fetch_threads and fetch_threads[0] != threading.get_ident(), "fetch on the bg worker"
    state.close()


# ---- revision-call cap (§7.4: <= 2 calls per cycle) ----------------------


def test_gates_fail_revision_then_final_no_third_call(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL, _REVISION])
    cycle = _mk_cycle(cfg, state, executor, complete, clock, passing=False)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(
        cycle, clock, client,
        lambda: len(state.events(kind=claude_worker.strategist.EVENT_CANDIDATE_REJECTED)) == 1,
    )
    assert len(complete.calls) == 2, "hard cap: proposal + one revision"
    assert cycle.stats.gate_failures == 2
    # The revision prompt carried the §7.4 gate summary + report.
    revision_prompt = complete.calls[1][1]
    assert "FAILED" in revision_prompt
    assert "pnl_positive=False" in revision_prompt
    assert '"all_passed": false' in revision_prompt
    assert '"auto-buy-low"' in revision_prompt, "prior rows ride the revision call"
    # Cycle over: archived with report, NO install, NO frames.
    final = json.loads(
        state.events(kind=claude_worker.strategist.EVENT_CANDIDATE_REJECTED)[0][3]
    )
    assert final["reason"] == "gates_failed_final"
    assert pathlib.Path(final["report"]).is_file()
    assert not cfg.ai_ruleset_dir.exists()
    assert state.events(kind=claude_worker.state.EVENT_FRAME_SENT) == []
    # Both candidates + both reports sit in the candidates dir.
    cand_dir = claude_worker.strategist.candidates_dir(cfg.db_path)
    assert len(list(cand_dir.glob("*.report.json"))) == 2
    ledger = state.events(kind=claude_worker.strategist.EVENT_STRATEGIST_CALL)
    assert [json.loads(row[3])["purpose"] for row in ledger] == ["proposal", "revision"]
    state.close()


# ---- promotion (§8.1/§8.2, design §12 promotion rows) --------------------


def test_full_cycle_auto_promotes_through_frozen_pair(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    executor: concurrent.futures.ThreadPoolExecutor,
) -> None:
    cfg = _mk_cfg(tmp_path, sock=fake_uds.sock_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL])
    cycle = _mk_cycle(cfg, state, executor, complete, clock, passing=True)
    client = claude_worker.uds.UdsClient(cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state)
    client.connect()
    client.send_heartbeat()  # serve invariant: heartbeat precedes payloads
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.promotions == 1)

    # §7.6: the Fable-5 call ran on the background worker with the
    # cache_control-marked static block.
    assert len(complete.calls) == 1
    system, prompt, thread_ident = complete.calls[0]
    assert system == claude_worker.strategist.system_blocks()
    assert "btc-daily -> sym 42" in prompt
    assert thread_ident != threading.get_ident()

    # Candidate written; artifact auto-installed at $AI_RULESET_DIR/<hash128>.json.
    cand_dir = claude_worker.strategist.candidates_dir(cfg.db_path)
    candidates = sorted(cand_dir.glob("*-*.json"))
    candidate_path = next(p for p in candidates if not p.name.endswith(".report.json"))
    full_hash, hash128 = claude_worker.backtest.ruleset_hashes(candidate_path)
    installed = cfg.ai_ruleset_dir / f"{hash128.hex()}.json"
    assert installed.read_bytes() == candidate_path.read_bytes()

    # Frames: heartbeat, then Stage + Commit through the FROZEN pair,
    # hash128 riding px/qty exactly as the wire convention pins.
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline and len(fake_uds.frames) < 3:
        time.sleep(0.01)
    assert fake_uds.errors == []
    assert [fake_uds.cmd_field(i, "kind") for i in range(3)] == [
        claude_worker.frames.KIND_HEARTBEAT,
        claude_worker.frames.KIND_RULESET_STAGE,
        claude_worker.frames.KIND_RULESET_COMMIT,
    ]
    px, qty = claude_worker.backtest.hash128_wire(hash128)
    for i in (1, 2):
        assert fake_uds.cmd_field(i, "px") == px
        assert fake_uds.cmd_field(i, "qty") == qty
        assert fake_uds.cmd_field(i, "venue") == claude_worker.frames.VENUE_AI
        assert fake_uds.cmd_field(i, "strategy_id") == claude_worker.frames.STRATEGY_SLOT_VM

    # Registry: staged + committed, author_mode 'auto', §8.2 attribution.
    row = state.ruleset_row(full_hash)
    assert row is not None
    assert row[3] is True and row[4] == "auto"
    assert row[5] is not None and row[6] is not None, "staged AND committed"
    assert state.ruleset_attribution(full_hash) == (
        "claude-fable-5",
        "fade the lagged PM quote",
    )

    # §7.5 ledger + promotion events.
    ledger = state.events(kind=claude_worker.strategist.EVENT_STRATEGIST_CALL)
    assert len(ledger) == 1
    detail = json.loads(ledger[0][3])
    assert detail["model"] == "claude-fable-5"
    assert detail["input_tokens"] == 111 and detail["output_tokens"] == 42
    assert detail["cache_read"] is False
    promo = state.events(kind=claude_worker.strategist.EVENT_PROMOTION)
    assert len(promo) == 1
    promo_detail = json.loads(promo[0][3])
    assert promo_detail["hash"] == full_hash
    assert promo_detail["hash128"] == hash128.hex()
    assert promo_detail["oos_net_pnl_usd"] == 5.0

    client.close()
    state.close()


def test_promote_waits_for_connection_then_delivers(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    executor: concurrent.futures.ThreadPoolExecutor,
) -> None:
    cfg = _mk_cfg(tmp_path, sock=fake_uds.sock_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL])
    cycle = _mk_cycle(cfg, state, executor, complete, clock, passing=True)
    client = claude_worker.uds.UdsClient(cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state)
    # DISCONNECTED: gates pass => install happens, frames wait (§7.6:
    # frames only ride the serve loop's live connection).
    clock.now_ns = 2_000_000_000
    _drive(
        cycle, clock, client,
        lambda: cfg.ai_ruleset_dir.exists() and any(cfg.ai_ruleset_dir.glob("*.json")),
    )
    for _ in range(10):
        cycle.maybe_run(clock(), client)
    assert cycle.stats.promotions == 0
    assert state.events(kind=claude_worker.state.EVENT_FRAME_SENT) == []
    # The engine returns: promote completes on the next ticks.
    client.connect()
    client.send_heartbeat()
    _drive(cycle, clock, client, lambda: cycle.stats.promotions == 1)
    row_hashes = [
        p.stem for p in cfg.ai_ruleset_dir.glob("*.json")
    ]
    assert len(row_hashes) == 1
    client.close()
    state.close()


# ---- serve composition (§9: the collaborator inside the real loop) -------


class _ServeFakeMessages:
    def __init__(self) -> None:
        self.calls: list[dict[str, object]] = []

    def create(self, **kwargs: object) -> types.SimpleNamespace:
        self.calls.append(kwargs)
        block = anthropic.types.TextBlock(type="text", text=_PROPOSAL, citations=None)
        usage = types.SimpleNamespace(
            input_tokens=500, output_tokens=90, cache_read_input_tokens=0,
            cache_creation_input_tokens=450,
        )
        return types.SimpleNamespace(content=[block], usage=usage)


class _ServeFakeClient:
    def __init__(self) -> None:
        self.messages = _ServeFakeMessages()


def test_serve_composes_research_cycle_end_to_end(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    (tmp_path / "replay" / "run-1").mkdir(parents=True)
    cfg = claude_worker.config.ServeConfig(
        ai_ingress_sock=fake_uds.sock_path,
        ai_ingress_hmac_key=tests.conftest.TEST_KEY,
        ai_ruleset_dir=tmp_path / "rulesets",
        replay_dir=tmp_path / "replay",
        db_path=tmp_path / "worker" / "state.db",
        features_dir=tmp_path / "features",
        market_map_path=tmp_path / "market-map.json",
        rss_feeds=(),  # no watcher traffic: the only LLM calls are the strategist's
        anthropic_api_key="sk-ant-test-000",
    )
    fake_llm = _ServeFakeClient()
    monkeypatch.setattr(
        claude_worker.llm,
        "make_client",
        lambda _unused_cfg: typing.cast(anthropic.Anthropic, fake_llm),
    )
    research_stats = claude_worker.daemon.ResearchStats()
    clock = _Clock()

    def ticking_clock() -> int:
        clock.now_ns += 500_000_000  # 0.5 s per loop iteration
        return clock.now_ns

    rc = claude_worker.daemon.serve(
        cfg,
        symbol_map={"btc-daily": 42},
        iterations=600,
        clock_ns=ticking_clock,
        sleep_fn=lambda _s: time.sleep(0.005),
        stats_out=claude_worker.daemon.ServeStats(),
        research_env={"CLAUDE_WORKER_STRATEGIST_INTERVAL_S": "1"},
        research_fetch_fn=lambda: True,
        research_run_backtest_fn=_fake_run_backtest(True),
        research_stats_out=research_stats,
    )
    assert rc == 0
    assert research_stats.cycles_started == 1
    assert research_stats.promotions == 1

    # The strategist call went out with system blocks + the 4096 budget.
    assert len(fake_llm.messages.calls) == 1
    sent = fake_llm.messages.calls[0]
    assert sent["model"] == "claude-fable-5"
    assert sent["max_tokens"] == claude_worker.llm.STRATEGIST_MAX_TOKENS
    assert sent["system"] == claude_worker.strategist.system_blocks()

    # Stage + Commit rode the serve connection; registry + attribution land.
    kinds = [fake_uds.cmd_field(i, "kind") for i in range(len(fake_uds.frames))]
    assert claude_worker.frames.KIND_RULESET_STAGE in kinds
    assert claude_worker.frames.KIND_RULESET_COMMIT in kinds
    assert kinds.index(claude_worker.frames.KIND_RULESET_STAGE) < kinds.index(
        claude_worker.frames.KIND_RULESET_COMMIT
    )
    assert fake_uds.errors == []
    state = claude_worker.state.State(cfg.db_path)
    installed = list(cfg.ai_ruleset_dir.glob("*.json"))
    assert len(installed) == 1
    full_hash, hash128 = claude_worker.backtest.ruleset_hashes(installed[0])
    assert installed[0].stem == hash128.hex()
    assert state.ruleset_attribution(full_hash) == (
        "claude-fable-5",
        "fade the lagged PM quote",
    )
    row = state.ruleset_row(full_hash)
    assert row is not None and row[6] is not None, "committed"
    detail = json.loads(
        state.events(kind=claude_worker.strategist.EVENT_STRATEGIST_CALL)[0][3]
    )
    assert detail["input_tokens"] == 500 and detail["cache_read"] is False
    state.close()


# ---- 8h-H5: §8.3 walk-forward monitor + rollback (design §12 monitor rows)

import claude_worker.monitor  # noqa: E402 — H5 suite section
import tests.craft  # noqa: E402

_H_NS = 3_600_000_000_000
_RECENT_EPOCH = 200 * _H_NS


class _DispatchBacktest:
    """Split-aware run_backtest double: 70/30 (the promotion gate) always
    PASSES; 0/100 (the monitor) BREACHES iff the scored artifact carries
    ``breach_marker`` (or raises when ``monitor_error`` is set). Records
    every call for window/split assertions."""

    def __init__(self, breach_marker: bytes, monitor_error: str | None = None) -> None:
        self._marker = breach_marker
        self._error = monitor_error
        self.calls: list[tuple[str, pathlib.Path, pathlib.Path]] = []

    def __call__(
        self,
        ruleset_path: pathlib.Path,
        replay_dir: pathlib.Path,
        split: str = "70/30",
    ) -> claude_worker.backtest.BacktestOutcome:
        self.calls.append((split, replay_dir, ruleset_path))
        assert split in ("70/30", claude_worker.monitor.MONITOR_SPLIT)
        if split == claude_worker.monitor.MONITOR_SPLIT and self._error is not None:
            raise claude_worker.backtest.BacktestError(self._error)
        breach = (
            split == claude_worker.monitor.MONITOR_SPLIT
            and self._marker in ruleset_path.read_bytes()
        )
        full_hash, _ = claude_worker.backtest.ruleset_hashes(ruleset_path)
        harness = claude_worker.backtest.HarnessReport(
            ruleset_hash=full_hash,
            split=split,
            oos_net_pnl_usd=-150.0 if breach else 5.0,
            oos_trades=60,
            oos_trading_days=3,
            oos_max_drawdown_usd=20.0,
            max_order_notional_usd=50.0,
            max_symbol_notional_usd=96.8,
            max_total_notional_usd=96.8,
        )
        thresholds = claude_worker.backtest.GateThresholds()
        gates = claude_worker.backtest.evaluate_gates(harness, thresholds)
        report_path = claude_worker.backtest.write_report(
            ruleset_path, full_hash, harness, gates, thresholds
        )
        return claude_worker.backtest.BacktestOutcome(
            all_passed=gates.all_passed, report_path=report_path, gates=gates, harness=harness
        )


def _mk_monitor_cycle(  # noqa: PLR0913 — test composition root
    cfg: claude_worker.config.BaseConfig,
    state: claude_worker.state.State,
    executor: concurrent.futures.Executor,
    complete: _CompleteFake,
    clock: _Clock,
    dispatch: _DispatchBacktest,
) -> claude_worker.daemon.ResearchCycle:
    return claude_worker.daemon.ResearchCycle(
        state,
        cfg,
        {"btc-daily": 42},
        complete,
        executor,
        fetch_fn=lambda: True,
        run_backtest_fn=dispatch,
        env={"CLAUDE_WORKER_STRATEGIST_INTERVAL_S": "1"},
        clock_ns=clock,
        wall_ns=lambda: _FIXED_WALL_NS,
    )


def _connected_client(
    fake_uds: tests.conftest.FakeUdsServer, state: claude_worker.state.State
) -> claude_worker.uds.UdsClient:
    client = claude_worker.uds.UdsClient(fake_uds.sock_path, tests.conftest.TEST_KEY, state)
    client.connect()
    client.send_heartbeat()
    return client


def _frame_kinds(server: tests.conftest.FakeUdsServer, count: int) -> list[int]:
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline and len(server.frames) < count:
        time.sleep(0.01)
    assert len(server.frames) == count, f"expected {count} frames, got {len(server.frames)}"
    return [server.cmd_field(i, "kind") for i in range(count)]


def test_monitor_noop_without_committed_ruleset(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake(["not json"])
    cycle = _mk_cycle(cfg, state, executor, complete, clock)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.candidates_rejected == 1)
    assert cycle.stats.monitor_runs == 0, "nothing committed => nothing to monitor"
    assert state.events(kind=claude_worker.strategist.EVENT_MONITOR_SKIP) == []
    state.close()


def test_promotion_arm_check_skips_on_thin_capture(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    executor: concurrent.futures.ThreadPoolExecutor,
) -> None:
    # The H4 promotion flow over the tickless default capture: the §8.3
    # arm check fires at cycle end and SKIPS below the 6 h floor.
    cfg = _mk_cfg(tmp_path, sock=fake_uds.sock_path)
    state = claude_worker.state.State(cfg.db_path)
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL])
    cycle = _mk_cycle(cfg, state, executor, complete, clock, passing=True)
    client = _connected_client(fake_uds, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.promotions == 1)
    assert cycle.stats.monitor_runs == 1, "post-promotion arm check ran"
    assert cycle.stats.monitor_skips == 1
    assert cycle.stats.rollbacks_triggered == 0
    skip = state.events(kind=claude_worker.strategist.EVENT_MONITOR_SKIP)
    assert len(skip) == 1
    detail = json.loads(skip[0][3])
    assert detail["reason"] == "coverage"
    assert detail["coverage_ns"] == 0
    assert detail["floor_ns"] == claude_worker.monitor.MONITOR_FLOOR_NS
    promo = json.loads(state.events(kind=claude_worker.strategist.EVENT_PROMOTION)[0][3])
    assert detail["active"] == promo["hash"], "the arm check scored the just-promoted hash"
    client.close()
    state.close()


def test_promote_arm_check_triggers_full_rollback(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    executor: concurrent.futures.ThreadPoolExecutor,
) -> None:
    """The §8.5-shaped path: promote on the gates, then the arm check's
    trailing window says the fresh ruleset LOSES — disable-5 BEFORE the
    frozen restage/commit of the prior, all in the pinned order."""
    cfg = _mk_cfg(tmp_path, sock=fake_uds.sock_path)
    state = claude_worker.state.State(cfg.db_path)
    prior_hash, _prior_path, prior_report = tests.craft.seed_committed_ruleset(
        state, tmp_path, "prior", "prior-row", staged_ts=100, committed_ts=200,
        model="claude-fable-5", thesis="prior thesis",
    )
    prior_report_bytes = prior_report.read_bytes()
    # >= 6 h of capture in one recent run (default run-1 stays tickless).
    tests.craft.write_run(cfg.replay_dir, _RECENT_EPOCH, [0, 8 * _H_NS])
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL])
    dispatch = _DispatchBacktest(breach_marker=b"auto-buy-low")
    cycle = _mk_monitor_cycle(cfg, state, executor, complete, clock, dispatch)
    client = _connected_client(fake_uds, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.rollbacks_completed == 1)

    assert cycle.stats.promotions == 1
    assert cycle.stats.monitor_runs == 1
    assert cycle.stats.rollbacks_triggered == 1
    assert cycle.stats.rollback_no_prior == 0

    # Frame order (§8.3 pinned): candidate Stage+Commit, then Disable-5
    # FIRST, then the prior's Stage+Commit through the frozen pair.
    kinds = _frame_kinds(fake_uds, 6)
    assert kinds == [
        claude_worker.frames.KIND_HEARTBEAT,
        claude_worker.frames.KIND_RULESET_STAGE,
        claude_worker.frames.KIND_RULESET_COMMIT,
        claude_worker.frames.KIND_DISABLE_STRATEGY,
        claude_worker.frames.KIND_RULESET_STAGE,
        claude_worker.frames.KIND_RULESET_COMMIT,
    ]
    assert fake_uds.errors == []
    assert fake_uds.cmd_field(3, "strategy_id") == claude_worker.frames.STRATEGY_SLOT_VM
    assert fake_uds.cmd_field(3, "venue") == claude_worker.frames.VENUE_AI
    assert fake_uds.cmd_field(3, "sym") == claude_worker.frames.SYMBOL_ID_NONE
    prior_px, prior_qty = claude_worker.backtest.hash128_wire(bytes.fromhex(prior_hash)[:16])
    assert fake_uds.cmd_field(4, "px") == prior_px
    assert fake_uds.cmd_field(4, "qty") == prior_qty
    assert fake_uds.cmd_field(5, "px") == prior_px

    # Monitor scored a COPY over the subset window dir with the carved split.
    monitor_calls = [c for c in dispatch.calls if c[0] == claude_worker.monitor.MONITOR_SPLIT]
    assert len(monitor_calls) == 1
    _split, window_dir, scored_path = monitor_calls[0]
    assert window_dir.name == "window", "tickless run-1 excluded => symlink subset"
    assert sorted(p.name for p in window_dir.iterdir()) == [f"run-{_RECENT_EPOCH}"]
    assert scored_path.name.startswith("active-"), "report-clobber protection: a copy was scored"

    # Events: rollback_triggered carries the metric values + the restage.
    promo = json.loads(state.events(kind=claude_worker.strategist.EVENT_PROMOTION)[0][3])
    trig = state.events(kind=claude_worker.strategist.EVENT_ROLLBACK_TRIGGERED)
    assert len(trig) == 1
    detail = json.loads(trig[0][3])
    assert detail["hash"] == promo["hash"]
    assert detail["restaged"] == prior_hash
    assert detail["net_pnl_usd"] == -150.0
    assert detail["net_trigger"] is True and detail["drawdown_trigger"] is False
    assert detail["coverage_ns"] == 8 * _H_NS
    assert state.events(kind=claude_worker.strategist.EVENT_ROLLBACK_NO_PRIOR) == []

    # Registry: the prior is committed again, its attribution PRESERVED
    # (the restage rode the frozen pair with no attribution — COALESCE).
    row = state.ruleset_row(prior_hash)
    assert row is not None and row[6] is not None and row[4] == "auto"
    assert state.ruleset_attribution(prior_hash) == ("claude-fable-5", "prior thesis")
    # Clobber protection held: the prior's PROMOTION report is byte-identical,
    # and the candidate's own gates-passed report still says 70/30 PASS.
    assert prior_report.read_bytes() == prior_report_bytes
    cand_reports = list(
        claude_worker.strategist.candidates_dir(cfg.db_path).glob("*.report.json")
    )
    assert len(cand_reports) == 1
    cand_report = json.loads(cand_reports[0].read_text())
    assert cand_report["split"] == "70/30" and cand_report["gates"]["all_passed"] is True
    monitor_report = json.loads(
        next(claude_worker.monitor.monitor_dir(cfg.db_path).glob("active-*.report.json")).read_text()
    )
    assert monitor_report["split"] == "0/100"

    # Next cycle (same capture => capture-skip) re-monitors: the events
    # ledger resolves ACTIVE = the restaged prior even though both rows
    # committed within the same wall second — no re-trigger.
    clock.now_ns = 4_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.monitor_runs == 2)
    assert cycle.stats.rollbacks_triggered == 1, "prior is active now and scores clean"
    assert cycle.stats.skips_no_capture == 1
    client.close()
    state.close()


def test_rollback_no_prior_is_disable_only_then_dark(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    executor: concurrent.futures.ThreadPoolExecutor,
) -> None:
    cfg = _mk_cfg(tmp_path, sock=fake_uds.sock_path)
    state = claude_worker.state.State(cfg.db_path)
    tests.craft.write_run(cfg.replay_dir, _RECENT_EPOCH, [0, 8 * _H_NS])
    clock = _Clock()
    complete = _CompleteFake([_PROPOSAL])
    dispatch = _DispatchBacktest(breach_marker=b"auto-buy-low")
    cycle = _mk_monitor_cycle(cfg, state, executor, complete, clock, dispatch)
    client = _connected_client(fake_uds, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.rollbacks_completed == 1)

    # §8.3 no-prior arm: disable only — NO second Stage/Commit pair.
    kinds = _frame_kinds(fake_uds, 4)
    assert kinds == [
        claude_worker.frames.KIND_HEARTBEAT,
        claude_worker.frames.KIND_RULESET_STAGE,
        claude_worker.frames.KIND_RULESET_COMMIT,
        claude_worker.frames.KIND_DISABLE_STRATEGY,
    ]
    trig = json.loads(
        state.events(kind=claude_worker.strategist.EVENT_ROLLBACK_TRIGGERED)[0][3]
    )
    assert trig["restaged"] is None
    no_prior = state.events(kind=claude_worker.strategist.EVENT_ROLLBACK_NO_PRIOR)
    assert len(no_prior) == 1
    assert cycle.stats.rollback_no_prior == 1

    # Dark guard: the disabled hash is not re-scored; no rollback spam.
    clock.now_ns = 4_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.skips_no_capture == 1)
    for _ in range(10):
        cycle.maybe_run(clock(), client)
    assert cycle.stats.monitor_runs == 1, "dark hash stands down until a NEW promotion"
    assert cycle.stats.rollbacks_triggered == 1
    assert len(fake_uds.frames) == 4
    client.close()
    state.close()


def test_rollback_waits_for_connection_and_blocks_cycles(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    executor: concurrent.futures.ThreadPoolExecutor,
) -> None:
    cfg = _mk_cfg(tmp_path, sock=fake_uds.sock_path)
    state = claude_worker.state.State(cfg.db_path)
    # Registry-seeded ACTIVE (no promotion event: the ledger is empty and
    # the registry head resolves) whose artifact breaches on the window.
    tests.craft.seed_committed_ruleset(
        state, tmp_path, "active", "bad-active", staged_ts=100, committed_ts=200
    )
    tests.craft.write_run(cfg.replay_dir, _RECENT_EPOCH, [0, 8 * _H_NS])
    clock = _Clock()
    complete = _CompleteFake(["not json"])  # the cycle itself ends quickly
    dispatch = _DispatchBacktest(breach_marker=b"bad-active")
    cycle = _mk_monitor_cycle(cfg, state, executor, complete, clock, dispatch)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.rollbacks_triggered == 1)
    for _ in range(10):
        cycle.maybe_run(clock(), client)
    assert cycle.stats.rollbacks_completed == 0
    assert state.events(kind=claude_worker.state.EVENT_FRAME_SENT) == []
    # A pending rollback blocks new cycles (the promote-pending discipline).
    clock.now_ns = 6_000_000_000
    for _ in range(10):
        cycle.maybe_run(clock(), client)
    assert cycle.stats.cycles_started == 1
    # The engine returns: disable-only delivers (no prior exists).
    client.connect()
    client.send_heartbeat()
    _drive(cycle, clock, client, lambda: cycle.stats.rollbacks_completed == 1)
    kinds = _frame_kinds(fake_uds, 2)
    assert kinds == [
        claude_worker.frames.KIND_HEARTBEAT,
        claude_worker.frames.KIND_DISABLE_STRATEGY,
    ]
    client.close()
    state.close()


def test_monitor_backtest_error_is_a_skip_not_a_rollback(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    (cfg.replay_dir / "run-1").rmdir()
    state = claude_worker.state.State(cfg.db_path)
    active_hash, _p, _r = tests.craft.seed_committed_ruleset(
        state, tmp_path, "active", "bad-active", staged_ts=100, committed_ts=200
    )
    tests.craft.write_run(cfg.replay_dir, _RECENT_EPOCH, [0, 8 * _H_NS])
    clock = _Clock()
    complete = _CompleteFake(["not json"])
    dispatch = _DispatchBacktest(breach_marker=b"bad-active", monitor_error="validator reject")
    cycle = _mk_monitor_cycle(cfg, state, executor, complete, clock, dispatch)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.monitor_skips == 1)
    detail = json.loads(state.events(kind=claude_worker.strategist.EVENT_MONITOR_SKIP)[0][3])
    assert detail["reason"] == "backtest_error"
    assert detail["active"] == active_hash
    assert "validator reject" in detail["error"]
    assert cycle.stats.rollbacks_triggered == 0
    assert state.events(kind=claude_worker.state.EVENT_FRAME_SENT) == []
    # Single run, no tickless sibling: the monitor handed the ROOT through.
    monitor_calls = [c for c in dispatch.calls if c[0] == claude_worker.monitor.MONITOR_SPLIT]
    assert len(monitor_calls) == 1 and monitor_calls[0][1] == cfg.replay_dir
    state.close()


def test_monitor_performance_feeds_next_digest(
    tmp_path: pathlib.Path, executor: concurrent.futures.ThreadPoolExecutor
) -> None:
    cfg = _mk_cfg(tmp_path)
    state = claude_worker.state.State(cfg.db_path)
    tests.craft.seed_committed_ruleset(
        state, tmp_path, "active", "good-active", staged_ts=100, committed_ts=200
    )
    tests.craft.write_run(cfg.replay_dir, _RECENT_EPOCH, [0, 8 * _H_NS])
    clock = _Clock()
    complete = _CompleteFake(["not json", "still not json"])
    dispatch = _DispatchBacktest(breach_marker=b"never-matches")
    cycle = _mk_monitor_cycle(cfg, state, executor, complete, clock, dispatch)
    client = _disconnected_client(cfg, state)
    clock.now_ns = 2_000_000_000
    _drive(cycle, clock, client, lambda: cycle.stats.monitor_runs == 1)
    assert "walk-forward" not in complete.calls[0][1], "no performance before the first score"
    # Fresh capture => a second full cycle; its digest carries §7.1's
    # walk-forward line from the monitor's last score.
    tests.craft.write_run(cfg.replay_dir, _RECENT_EPOCH + 10 * _H_NS, [0, 7 * _H_NS])
    clock.now_ns = 4_000_000_000
    _drive(cycle, clock, client, lambda: len(complete.calls) == 2)
    second_prompt = complete.calls[1][1]
    assert "ACTIVE RULESET WALK-FORWARD" in second_prompt
    assert "verdict=holding" in second_prompt
    state.close()
