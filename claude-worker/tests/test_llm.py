"""llm.py — the SDK seam. FakeClient only; no live calls anywhere
(§11: SDK mocked at the llm.py seam).

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import types
import typing

import anthropic

import claude_worker.config
import claude_worker.llm
import tests.conftest


def _cast(client: tests.conftest.FakeClient) -> anthropic.Anthropic:
    return typing.cast(anthropic.Anthropic, client)


def test_complete_returns_first_text_block() -> None:
    fake = tests.conftest.FakeClient(lambda model, prompt: f"{model}::{prompt}")
    out = claude_worker.llm.complete(_cast(fake), "haiku-test", "ping")
    assert out == "haiku-test::ping"
    assert fake.calls == [("haiku-test", "ping")]


def test_complete_empty_content_is_empty_string() -> None:
    fake = tests.conftest.FakeClient(lambda _m, _p: "unused")

    def empty_create(**_kwargs: object) -> types.SimpleNamespace:
        return types.SimpleNamespace(content=[])

    fake.messages.create = empty_create  # type: ignore[method-assign]
    assert claude_worker.llm.complete(_cast(fake), "m", "p") == ""


def test_complete_fn_for_adapts_seam() -> None:
    fake = tests.conftest.FakeClient(lambda model, prompt: f"r:{model}:{prompt}")
    fn = claude_worker.llm.complete_fn_for(_cast(fake))
    assert fn("sonnet-test", "classify") == "r:sonnet-test:classify"
    assert fake.calls == [("sonnet-test", "classify")]


def test_make_client_uses_serve_config_key(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.config.ServeConfig(
        ai_ingress_sock=tmp_path / "ai.sock",
        ai_ingress_hmac_key=bytes(32),
        ai_ruleset_dir=tmp_path / "rulesets",
        replay_dir=tmp_path / "replay",
        db_path=tmp_path / "state.db",
        features_dir=tmp_path / "features",
        rss_feeds=(),
        anthropic_api_key="sk-ant-test-not-a-real-key",
    )
    client = claude_worker.llm.make_client(cfg)
    assert isinstance(client, anthropic.Anthropic)
    assert client.api_key == "sk-ant-test-not-a-real-key"
