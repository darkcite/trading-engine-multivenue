"""daemon.py serve loop — the §11 composed-iteration test: canned feeds
(httpx.MockTransport) + mocked LLM (FakeClient at the llm.py seam) +
fake UDS server. Asserts dedupe honored, prompt-cache hit on identical
content, heartbeat emitted, clean SIGTERM shutdown, and survival with
the engine socket absent.

Signals are delivered to OUR OWN pid only (session hygiene: by-PID).

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import os
import pathlib
import random
import signal
import threading
import time
import typing

import anthropic
import httpx
import pytest

import claude_worker.config
import claude_worker.daemon
import claude_worker.frames
import claude_worker.llm
import claude_worker.state
import tests.conftest

FEED_URL = "https://news.example/rss"

# g1 is pre-seeded in SQLite (dedupe assertion); g3/g4 share content
# (prompt-cache assertion).
FEED_XML = """<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Example</title>
<item><guid>g1</guid><title>Bitcoin ETF approved</title>
<description>The SEC approved a spot ETF.</description></item>
<item><guid>g3</guid><title>Solana upgrade ships</title>
<description>Major throughput gains.</description></item>
<item><guid>g4</guid><title>Solana upgrade ships</title>
<description>Major throughput gains.</description></item>
</channel></rss>
"""


def _respond(_model: str, prompt: str) -> str:
    if "triage tagger" in prompt:
        return json.dumps({"family": "crypto", "impact": "high", "reason": "big"})
    return json.dumps(
        {"market": "BTC-DAILY", "direction": "up", "confidence": 0.9, "half_life_s": 600}
    )


def _cfg(sock: pathlib.Path, tmp_path: pathlib.Path) -> claude_worker.config.ServeConfig:
    return claude_worker.config.ServeConfig(
        ai_ingress_sock=sock,
        ai_ingress_hmac_key=tests.conftest.TEST_KEY,
        ai_ruleset_dir=tmp_path / "rulesets",
        replay_dir=tmp_path / "replay",
        db_path=tmp_path / "state.db",
        features_dir=tmp_path / "features",
        rss_feeds=(FEED_URL,),
        anthropic_api_key="sk-ant-test-000",
    )


def _http() -> httpx.Client:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text=FEED_XML)

    return httpx.Client(transport=httpx.MockTransport(handler))


def _patch_llm(monkeypatch: pytest.MonkeyPatch, fake: tests.conftest.FakeClient) -> None:
    monkeypatch.setattr(
        claude_worker.llm,
        "make_client",
        lambda _unused_cfg: typing.cast(anthropic.Anthropic, fake),
    )


def _wait_for_frames(server: tests.conftest.FakeUdsServer, count: int) -> None:
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if len(server.frames) >= count:
            return
        time.sleep(0.01)
    raise AssertionError(f"expected {count} frames, got {len(server.frames)}")


def test_serve_one_composed_iteration(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cfg = _cfg(fake_uds.sock_path, tmp_path)
    # Pre-seed dedupe: g1 is already known (§11 "dedupe honored").
    seeded = claude_worker.state.State(cfg.db_path)
    seeded.mark_seen(FEED_URL, "g1", 0)
    seeded.close()

    fake_llm = tests.conftest.FakeClient(_respond)
    _patch_llm(monkeypatch, fake_llm)
    stats = claude_worker.daemon.ServeStats()
    prev_term = signal.getsignal(signal.SIGTERM)

    rc = claude_worker.daemon.serve(
        cfg,
        symbol_map={"BTC-DAILY": 7},
        iterations=1,
        http_client=_http(),
        clock_ns=lambda: 0,
        sleep_fn=lambda _s: None,
        rng=random.Random(1),
        stats_out=stats,
    )
    assert rc == 0
    assert signal.getsignal(signal.SIGTERM) is prev_term  # handlers restored

    # Heartbeat + two SetBias frames (g3 real, g4 via cache).
    _wait_for_frames(fake_uds, 3)
    assert fake_uds.errors == []
    assert fake_uds.cmd_field(0, "kind") == claude_worker.frames.KIND_HEARTBEAT
    assert fake_uds.cmd_field(1, "kind") == claude_worker.frames.KIND_SET_BIAS
    assert fake_uds.cmd_field(2, "kind") == claude_worker.frames.KIND_SET_BIAS
    assert fake_uds.cmd_field(1, "sym") == 7
    assert fake_uds.cmd_field(1, "px") == 18_000

    assert stats.iterations == 1
    assert stats.heartbeats == 1
    assert stats.labels_emitted == 2
    assert stats.labels_dropped_disconnected == 0

    # Cache hit on the identical second prompt (§11): one real triage +
    # one real label call TOTAL — g4 was served from prompt_cache; and
    # the deduped g1 never reached a model (no Bitcoin prompt).
    assert len(fake_llm.calls) == 2
    for _model, prompt in fake_llm.calls:
        assert "Bitcoin" not in prompt

    # SQLite flushed and reopenable: send-time records + cache rows exist.
    reopened = claude_worker.state.State(cfg.db_path)
    sends = reopened.events(claude_worker.state.EVENT_FRAME_SENT)
    assert len(sends) == 3
    reopened.close()


def test_serve_sigterm_clean_shutdown(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cfg = _cfg(fake_uds.sock_path, tmp_path)
    fake_llm = tests.conftest.FakeClient(_respond)
    _patch_llm(monkeypatch, fake_llm)
    prev_term = signal.getsignal(signal.SIGTERM)

    # By-PID signal to our own process only (hygiene rule).
    timer = threading.Timer(0.4, os.kill, args=(os.getpid(), signal.SIGTERM))
    timer.start()
    try:
        rc = claude_worker.daemon.serve(
            cfg,
            symbol_map={"BTC-DAILY": 7},
            http_client=_http(),
            rng=random.Random(1),
        )
    finally:
        timer.cancel()
    assert rc == 0
    assert signal.getsignal(signal.SIGTERM) is prev_term

    # Shutdown flushed SQLite and closed the UDS: at least the first
    # heartbeat made it out and is recorded.
    _wait_for_frames(fake_uds, 1)
    assert fake_uds.cmd_field(0, "kind") == claude_worker.frames.KIND_HEARTBEAT
    reopened = claude_worker.state.State(cfg.db_path)
    assert len(reopened.events(claude_worker.state.EVENT_FRAME_SENT)) >= 1
    reopened.close()


def test_serve_survives_engine_absent(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # No listener at the socket path: connects fail, labels are dropped
    # (counted), the loop stays alive, exit stays clean — fail-safe §5.4.
    cfg = _cfg(tmp_path / "absent.sock", tmp_path)
    fake_llm = tests.conftest.FakeClient(_respond)
    _patch_llm(monkeypatch, fake_llm)
    stats = claude_worker.daemon.ServeStats()

    rc = claude_worker.daemon.serve(
        cfg,
        symbol_map={"BTC-DAILY": 7},
        iterations=2,
        http_client=_http(),
        clock_ns=lambda: 0,
        sleep_fn=lambda _s: None,
        rng=random.Random(1),
        stats_out=stats,
    )
    assert rc == 0
    assert stats.connect_failures == 2
    assert stats.labels_dropped_disconnected == 3  # g1+g3+g4 labels all dropped
    assert stats.labels_emitted == 0
    assert stats.heartbeats == 0
