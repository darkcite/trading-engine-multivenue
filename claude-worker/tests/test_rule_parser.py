"""Tests for claude_worker.rule_parser.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import typing

import pytest

import claude_worker.config
import claude_worker.rule_parser

import tests.conftest as conftest  # for type-alias only


_VALID_RESPONSE: str = json.dumps(
    {
        "name": "btc_spot_spread",
        "family": "crypto",
        "trigger": "binance/polymarket spread > 40bps for 500ms",
        "edge_bps": 35,
        "horizon_ms": 1500,
        "max_risk_usd": 500,
    }
)


def test_parse_note_happy_path(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    fake = fake_client_factory([_VALID_RESPONSE])
    rule = claude_worker.rule_parser.parse_note(tmp_cfg, "buy BTC on spread")

    assert rule.name == "btc_spot_spread"
    assert rule.family == "crypto"
    assert rule.edge_bps == 35
    assert rule.horizon_ms == 1500
    assert rule.max_risk_usd == 500

    assert len(fake.messages.calls) == 1
    assert fake.messages.calls[0]["model"] == claude_worker.config.MODEL_REASONING


def test_parse_note_rejects_malformed_json(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    fake_client_factory(["not-json-at-all"])
    with pytest.raises(claude_worker.rule_parser.RuleParseError, match="not valid JSON"):
        claude_worker.rule_parser.parse_note(tmp_cfg, "x")


def test_parse_note_rejects_unknown_family(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    bad = json.dumps(
        {
            "name": "x",
            "family": "martian-futures",
            "trigger": "t",
            "edge_bps": 1,
            "horizon_ms": 1,
            "max_risk_usd": 1,
        }
    )
    fake_client_factory([bad])
    with pytest.raises(claude_worker.rule_parser.RuleParseError, match="family"):
        claude_worker.rule_parser.parse_note(tmp_cfg, "x")


def test_parse_note_rejects_empty_name(
    tmp_cfg: claude_worker.config.WorkerConfig,
    fake_client_factory: typing.Callable[[list[str]], "conftest.FakeClient"],
) -> None:
    bad = json.dumps(
        {
            "name": "",
            "family": "crypto",
            "trigger": "t",
            "edge_bps": 1,
            "horizon_ms": 1,
            "max_risk_usd": 1,
        }
    )
    fake_client_factory([bad])
    with pytest.raises(claude_worker.rule_parser.RuleParseError, match="name"):
        claude_worker.rule_parser.parse_note(tmp_cfg, "x")


def test_parse_note_rejects_bool_as_int() -> None:
    """bool is-a int in Python — guard against it sneaking in."""
    bad = json.dumps(
        {
            "name": "x",
            "family": "crypto",
            "trigger": "t",
            "edge_bps": True,  # pytest's json will emit this as true
            "horizon_ms": 1,
            "max_risk_usd": 1,
        }
    )
    with pytest.raises(claude_worker.rule_parser.RuleParseError, match="bool"):
        claude_worker.rule_parser._parse_rule(bad)


def test_parse_note_rejects_negative_edge() -> None:
    bad = json.dumps(
        {
            "name": "x",
            "family": "crypto",
            "trigger": "t",
            "edge_bps": -5,
            "horizon_ms": 1,
            "max_risk_usd": 1,
        }
    )
    with pytest.raises(claude_worker.rule_parser.RuleParseError, match="edge_bps"):
        claude_worker.rule_parser._parse_rule(bad)


def test_parse_note_rejects_zero_horizon() -> None:
    bad = json.dumps(
        {
            "name": "x",
            "family": "crypto",
            "trigger": "t",
            "edge_bps": 1,
            "horizon_ms": 0,
            "max_risk_usd": 1,
        }
    )
    with pytest.raises(claude_worker.rule_parser.RuleParseError, match="horizon_ms"):
        claude_worker.rule_parser._parse_rule(bad)


def test_write_artifact_json_round_trip(tmp_path: pathlib.Path) -> None:
    rules = [
        claude_worker.rule_parser.StrategyRule(
            name="r1",
            family="crypto",
            trigger="t1",
            edge_bps=10,
            horizon_ms=500,
            max_risk_usd=100,
        ),
        claude_worker.rule_parser.StrategyRule(
            name="r2",
            family="sports",
            trigger="t2",
            edge_bps=25,
            horizon_ms=2000,
            max_risk_usd=250,
        ),
    ]
    out = tmp_path / "rules" / "r.json"
    claude_worker.rule_parser.write_artifact(rules, out)

    data = json.loads(out.read_text("utf-8"))
    assert isinstance(data, list)
    assert len(data) == 2
    assert data[0]["name"] == "r1"
    assert data[1]["max_risk_usd"] == 250
