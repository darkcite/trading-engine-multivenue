"""Tests for claude_worker.topic_tagger.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import typing

import pytest

import claude_worker.config
import claude_worker.topic_tagger

import tests.conftest as conftest  # for type-alias only


def test_tag_batch_happy_path(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    fake = fake_client_factory(
        [
            '{"family":"crypto","impact":"high","reason":"ETF inflow"}',
            '{"family":"politics","impact":"med","reason":"primary result"}',
        ]
    )

    out = claude_worker.topic_tagger.tag_batch(
        tmp_cfg,
        [("a1", "SEC approves spot ETH ETF"), ("p7", "NH primary called early")],
    )

    assert len(out) == 2
    assert out[0].id == "a1"
    assert out[0].family == "crypto"
    assert out[0].impact == "high"
    assert out[0].reason == "ETF inflow"

    assert out[1].id == "p7"
    assert out[1].family == "politics"
    assert out[1].impact == "med"

    # Two completions were issued, both with the Haiku model.
    assert len(fake.messages.calls) == 2
    assert fake.messages.calls[0]["model"] == claude_worker.config.MODEL_BULK
    assert fake.messages.calls[1]["model"] == claude_worker.config.MODEL_BULK


def test_tag_batch_tolerates_malformed_json(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    fake_client_factory(["totally not json"])
    out = claude_worker.topic_tagger.tag_batch(tmp_cfg, [("x", "whatever")])
    assert len(out) == 1
    assert out[0].family == "other"
    assert out[0].impact == "low"
    assert out[0].reason == "malformed-json"


def test_tag_batch_rejects_unknown_family(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    fake_client_factory(
        ['{"family":"aliens","impact":"bogus","reason":"x"}']
    )
    out = claude_worker.topic_tagger.tag_batch(tmp_cfg, [("x", "whatever")])
    assert out[0].family == "other"
    assert out[0].impact == "low"


def test_tag_batch_truncates_long_reason(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    long_reason = "x" * 500
    fake_client_factory(
        [json.dumps({"family": "macro", "impact": "high", "reason": long_reason})]
    )
    out = claude_worker.topic_tagger.tag_batch(tmp_cfg, [("m", "CPI release")])
    assert len(out[0].reason) == 120


def test_write_artifact_ndjson_round_trip(
    tmp_path: pathlib.Path,
) -> None:
    tags = [
        claude_worker.topic_tagger.TopicTag(id="a", family="crypto", impact="high", reason="x"),
        claude_worker.topic_tagger.TopicTag(id="b", family="macro", impact="low", reason="y"),
    ]
    out = tmp_path / "sub" / "tags.ndjson"
    claude_worker.topic_tagger.write_artifact(tags, out)

    lines = out.read_text("utf-8").splitlines()
    assert len(lines) == 2
    obj0 = json.loads(lines[0])
    assert obj0 == {"id": "a", "family": "crypto", "impact": "high", "reason": "x"}


def test_tag_batch_uses_explicit_model_override(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    fake = fake_client_factory(
        ['{"family":"other","impact":"low","reason":"z"}']
    )
    claude_worker.topic_tagger.tag_batch(
        tmp_cfg, [("x", "y")], model="claude-sonnet-4-6"
    )
    assert fake.messages.calls[0]["model"] == "claude-sonnet-4-6"


@pytest.mark.network
def test_no_real_network_is_hit(monkeypatch: pytest.MonkeyPatch) -> None:
    """Safety net: if anyone accidentally removes the fake, fail loudly.

    We install a make_client that raises, and confirm tag_batch propagates it
    rather than silently calling the real SDK.
    """
    import claude_worker.anthropic_client as ac

    def boom(api_key: str) -> typing.Any:
        raise RuntimeError("real network attempted — fake not installed")

    monkeypatch.setattr(ac, "make_client", boom)

    cfg = claude_worker.config.WorkerConfig(
        anthropic_api_key="sk-ant-x" + "0" * 20,
        artifacts_dir=pathlib.Path("/tmp/a"),
        log_dir=pathlib.Path("/tmp/b"),
    )
    with pytest.raises(RuntimeError, match="real network"):
        claude_worker.topic_tagger.tag_batch(cfg, [("i", "t")])
