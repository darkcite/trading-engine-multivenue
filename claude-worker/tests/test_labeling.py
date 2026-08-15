"""labeling.py strict parsers (§9.1 carried patterns: tagger vocab,
bool-rejecting numeric coercion) + prompt content pins.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json

import claude_worker.labeling

SYMBOL_MAP = {"BTC-DAILY": 7, "ETH-WEEKLY": 9}


# ---- triage parsing ---------------------------------------------------------


def test_parse_triage_valid() -> None:
    raw = json.dumps({"family": "crypto", "impact": "high", "reason": "ETF approved"})
    result = claude_worker.labeling.parse_triage(raw)
    assert result == claude_worker.labeling.TriageResult("crypto", "high", "ETF approved")


def test_parse_triage_rejects_bad_vocab() -> None:
    bad_family = json.dumps({"family": "weather", "impact": "low", "reason": "x"})
    assert claude_worker.labeling.parse_triage(bad_family) is None
    bad_impact = json.dumps({"family": "macro", "impact": "huge", "reason": "x"})
    assert claude_worker.labeling.parse_triage(bad_impact) is None


def test_parse_triage_rejects_shape_violations() -> None:
    assert claude_worker.labeling.parse_triage("not json") is None
    assert claude_worker.labeling.parse_triage("[1, 2]") is None
    missing = json.dumps({"family": "crypto", "impact": "low"})
    assert claude_worker.labeling.parse_triage(missing) is None
    extra = json.dumps({"family": "crypto", "impact": "low", "reason": "x", "note": "y"})
    assert claude_worker.labeling.parse_triage(extra) is None
    long_reason = json.dumps(
        {
            "family": "crypto",
            "impact": "low",
            "reason": "r" * (claude_worker.labeling.REASON_MAX + 1),
        }
    )
    assert claude_worker.labeling.parse_triage(long_reason) is None
    nonstr_reason = json.dumps({"family": "crypto", "impact": "low", "reason": 5})
    assert claude_worker.labeling.parse_triage(nonstr_reason) is None


# ---- label parsing -----------------------------------------------------------


def _label_obj(**overrides: object) -> str:
    obj: dict[str, object] = {
        "market": "BTC-DAILY",
        "direction": "up",
        "confidence": 0.9,
        "half_life_s": 600,
    }
    obj.update(overrides)
    return json.dumps(obj)


def test_parse_label_valid() -> None:
    label, malformed = claude_worker.labeling.parse_label(_label_obj(), SYMBOL_MAP)
    assert malformed is False
    assert label is not None
    assert label.sym == 7
    assert label.direction == "up"
    assert label.confidence == 0.9
    assert label.half_life_s == 600.0


def test_parse_label_int_confidence_coerces() -> None:
    label, malformed = claude_worker.labeling.parse_label(_label_obj(confidence=1), SYMBOL_MAP)
    assert malformed is False
    assert label is not None
    assert label.confidence == 1.0


def test_parse_label_explicit_pass() -> None:
    label, malformed = claude_worker.labeling.parse_label(json.dumps({"market": None}), SYMBOL_MAP)
    assert label is None
    assert malformed is False
    # Pass shape tolerates schema-echo extra keys.
    echoed = json.dumps({"market": None, "direction": "up"})
    label, malformed = claude_worker.labeling.parse_label(echoed, SYMBOL_MAP)
    assert label is None
    assert malformed is False


def test_parse_label_bool_rejected() -> None:
    # §9.1 bool-rejecting coercion: True must not pass as 1.0.
    label, malformed = claude_worker.labeling.parse_label(_label_obj(confidence=True), SYMBOL_MAP)
    assert label is None
    assert malformed is True
    label, malformed = claude_worker.labeling.parse_label(_label_obj(half_life_s=True), SYMBOL_MAP)
    assert label is None
    assert malformed is True


def test_parse_label_bounds() -> None:
    for bad in (_label_obj(confidence=1.5), _label_obj(confidence=-0.1)):
        label, malformed = claude_worker.labeling.parse_label(bad, SYMBOL_MAP)
        assert label is None
        assert malformed is True
    for bad in (_label_obj(half_life_s=0), _label_obj(half_life_s=-5)):
        label, malformed = claude_worker.labeling.parse_label(bad, SYMBOL_MAP)
        assert label is None
        assert malformed is True


def test_parse_label_rejects_unmapped_market_and_bad_direction() -> None:
    label, malformed = claude_worker.labeling.parse_label(
        _label_obj(market="DOGE-HOURLY"), SYMBOL_MAP
    )
    assert label is None
    assert malformed is True
    label, malformed = claude_worker.labeling.parse_label(
        _label_obj(direction="sideways"), SYMBOL_MAP
    )
    assert label is None
    assert malformed is True


def test_parse_label_rejects_shape_violations() -> None:
    label, malformed = claude_worker.labeling.parse_label("not json", SYMBOL_MAP)
    assert label is None
    assert malformed is True
    extra = json.dumps(
        {
            "market": "BTC-DAILY",
            "direction": "up",
            "confidence": 0.5,
            "half_life_s": 60,
            "note": "x",
        }
    )
    label, malformed = claude_worker.labeling.parse_label(extra, SYMBOL_MAP)
    assert label is None
    assert malformed is True


# ---- prompt content pins -------------------------------------------------------


def test_triage_prompt_carries_vocab_and_item() -> None:
    prompt = claude_worker.labeling.build_triage_prompt("Title X", "Body Y")
    for family in claude_worker.labeling.FAMILIES:
        assert family in prompt
    for impact in claude_worker.labeling.IMPACTS:
        assert impact in prompt
    assert "Title X" in prompt
    assert "Body Y" in prompt


def test_label_prompt_carries_market_list() -> None:
    prompt = claude_worker.labeling.build_label_prompt("T", "B", ("BTC-DAILY", "ETH-WEEKLY"))
    assert "- BTC-DAILY" in prompt
    assert "- ETH-WEEKLY" in prompt
    assert "half_life_s" in prompt


def test_prompt_versions_distinct() -> None:
    assert (
        claude_worker.labeling.TRIAGE_PROMPT_VERSION != claude_worker.labeling.LABEL_PROMPT_VERSION
    )
