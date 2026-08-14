"""Shared pytest fixtures.

We mock the Anthropic SDK at the ``claude_worker.anthropic_client.make_client``
boundary. No real network calls ever happen in tests — this mirrors the
hard rule in CLAUDE.md: "No live Anthropic API calls in tests."

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import pathlib
import typing

import pytest

import claude_worker.anthropic_client
import claude_worker.config


@dataclasses.dataclass
class _FakeUsage:
    input_tokens: int = 0
    output_tokens: int = 0


@dataclasses.dataclass
class _FakeBlock:
    type: str
    text: str


@dataclasses.dataclass
class _FakeMessage:
    content: list[_FakeBlock]
    stop_reason: str
    usage: _FakeUsage


class _FakeMessages:
    """Stub ``client.messages`` that returns pre-programmed text responses."""

    def __init__(self, responses: list[str]) -> None:
        self._responses: list[str] = list(responses)
        self.calls: list[dict[str, typing.Any]] = []

    def create(self, **kwargs: typing.Any) -> _FakeMessage:
        self.calls.append(kwargs)
        if not self._responses:
            raise AssertionError("FakeMessages: no more programmed responses")
        text = self._responses.pop(0)
        return _FakeMessage(
            content=[_FakeBlock(type="text", text=text)],
            stop_reason="end_turn",
            usage=_FakeUsage(input_tokens=10, output_tokens=20),
        )


class FakeClient:
    """Minimal fake matching the subset of ``anthropic.Anthropic`` we use."""

    def __init__(self, responses: list[str]) -> None:
        self.messages: _FakeMessages = _FakeMessages(responses)


@pytest.fixture()
def tmp_cfg(tmp_path: pathlib.Path) -> claude_worker.config.WorkerConfig:
    """A valid config with temporary directories — no real secret."""
    return claude_worker.config.WorkerConfig(
        anthropic_api_key="sk-ant-test-0000000000000000000000000000",
        artifacts_dir=tmp_path / "artifacts",
        log_dir=tmp_path / "logs",
    )


@pytest.fixture()
def fake_client_factory(
    monkeypatch: pytest.MonkeyPatch,
) -> typing.Callable[[list[str]], FakeClient]:
    """Monkey-patches ``make_client`` to return a ``FakeClient``.

    Usage::

        fake = fake_client_factory(["{...}", "{...}"])
        # now any module that calls make_client(...) gets this fake
    """

    def install(responses: list[str]) -> FakeClient:
        fake = FakeClient(responses)
        monkeypatch.setattr(
            claude_worker.anthropic_client,
            "make_client",
            lambda api_key: fake,
        )
        return fake

    return install
