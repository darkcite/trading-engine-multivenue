"""Prompt formats + strict parsers for triage (Haiku) and labeling
(Sonnet) — design §5.1 news_watcher pipeline, §9.1 carried patterns.

Two prompt schemas live here:

- **triage** — the §9.1 tagger vocabulary carried forward verbatim:
  ``family ∈ {crypto, politics, sports, macro, other}``,
  ``impact ∈ {low, med, high}``, ``reason`` ≤ 120 chars.
- **label** — the §5.1 labeling schema: market-mapped (the model must
  pick from the caller-provided market list; mapping to ``SymbolId`` is
  ours, never the model's), direction, confidence, half-life. The §9.1
  rule-extractor parsing style is carried: strict JSON, exact key set,
  bounds enforced, and **bool-rejecting numeric coercion** (``True`` is
  an ``int`` subclass in Python — a hallucinated boolean must not pass
  as ``1.0``).

Malformed model output NEVER raises out of the parsers — they return
``None`` (plus a malformed flag for labels) and the caller counts it;
the daemon loop must survive any garbage (§5.1).

Model calls themselves happen in ``feeds.py`` through an injected
``complete_fn`` — this module builds prompts and parses output only, so
it is trivially testable and SDK-free.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import typing

# ---- §9.1 tagger vocabulary (wire-stable across prompt versions) ----

FAMILIES: tuple[str, ...] = ("crypto", "politics", "sports", "macro", "other")
IMPACTS: tuple[str, ...] = ("low", "med", "high")
REASON_MAX: int = 120

DIRECTIONS: tuple[str, ...] = ("up", "down")

# Version strings feed the prompt_cache key (state.py §5.3): bump on ANY
# prompt-text change so stale cached responses cannot leak across versions.
TRIAGE_PROMPT_VERSION: str = "triage-v1"
LABEL_PROMPT_VERSION: str = "label-v1"


class TriageResult(typing.NamedTuple):
    """Parsed triage output (§9.1 tagger schema)."""

    family: str
    impact: str
    reason: str


class Label(typing.NamedTuple):
    """Parsed label: symbol-mapped, direction ∈ {up, down}, confidence in
    [0, 1], half-life in seconds (> 0) — the §5.1 labeling schema."""

    sym: int
    direction: str
    confidence: float
    half_life_s: float


def build_triage_prompt(title: str, text: str) -> str:
    """The Haiku triage prompt (tagger vocab, §9.1)."""
    return (
        "You are a news triage tagger for a trading research system.\n"
        "Classify the news item below. Respond with EXACTLY one JSON object"
        " and nothing else:\n"
        '{"family": "crypto"|"politics"|"sports"|"macro"|"other",'
        ' "impact": "low"|"med"|"high",'
        f' "reason": "string, at most {REASON_MAX} chars"}}\n'
        f"Title: {title}\n"
        f"Text: {text}\n"
    )


def build_label_prompt(title: str, text: str, markets: typing.Sequence[str]) -> str:
    """The Sonnet labeling prompt. ``markets`` is the closed choice set —
    the model never invents identifiers (market-mapped, §5.1)."""
    market_lines = "\n".join(f"- {name}" for name in markets)
    return (
        "You are a market-impact labeler for a trading research system.\n"
        "Given the news item, pick the ONE affected market from the list"
        " below, or pass.\n"
        f"Markets:\n{market_lines}\n"
        "Respond with EXACTLY one JSON object and nothing else:\n"
        '{"market": "<name from the list>"|null, "direction": "up"|"down",'
        ' "confidence": <number 0..1>, "half_life_s": <seconds, > 0>}\n'
        "A null market means no listed market is affected (omit the other"
        " keys in that case).\n"
        f"Title: {title}\n"
        f"Text: {text}\n"
    )


def _load_json_object(raw: str) -> dict[str, object] | None:
    """One strict JSON object, or None. Never raises."""
    try:
        obj = json.loads(raw)
    except ValueError, TypeError:
        return None
    if not isinstance(obj, dict):
        return None
    return typing.cast(dict[str, object], obj)


def _number(value: object) -> float | None:
    """Bool-rejecting numeric coercion (§9.1 rule-extractor pattern):
    ``bool`` is an ``int`` subclass and must not pass."""
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    return None


def parse_triage(raw: str) -> TriageResult | None:
    """Strict parse of triage output; None on ANY deviation (counted by
    the caller, never fatal)."""
    obj = _load_json_object(raw)
    if obj is None or set(obj) != {"family", "impact", "reason"}:
        return None
    family = obj["family"]
    impact = obj["impact"]
    reason = obj["reason"]
    if family not in FAMILIES or impact not in IMPACTS:
        return None
    if not isinstance(reason, str) or len(reason) > REASON_MAX:
        return None
    return TriageResult(family=str(family), impact=str(impact), reason=reason)


def parse_label(raw: str, symbol_map: dict[str, int]) -> tuple[Label | None, bool]:
    """Strict parse of label output.

    Returns ``(label, malformed)``: ``(None, False)`` is an EXPLICIT pass
    (``"market": null`` — a valid model answer), ``(None, True)`` is
    malformed output, ``(label, False)`` is a usable label. Unmapped
    market names count as malformed — the closed list was in the prompt.
    """
    obj = _load_json_object(raw)
    if obj is None:
        return None, True
    if obj.get("market", "") is None:
        # Explicit pass: {"market": null} alone (extra keys tolerated on
        # the pass shape — some models echo the schema).
        return None, False
    label = _validate_label(obj, symbol_map)
    return label, label is None


def _validate_label(obj: dict[str, object], symbol_map: dict[str, int]) -> Label | None:
    """Field-level validation for the non-pass label shape."""
    if set(obj) != {"market", "direction", "confidence", "half_life_s"}:
        return None
    market = obj["market"]
    direction = obj["direction"]
    confidence = _number(obj["confidence"])
    half_life_s = _number(obj["half_life_s"])
    if not isinstance(market, str) or market not in symbol_map:
        return None
    if direction not in DIRECTIONS:
        return None
    if confidence is None or not 0.0 <= confidence <= 1.0:
        return None
    if half_life_s is None or half_life_s <= 0.0:
        return None
    return Label(
        sym=symbol_map[market],
        direction=str(direction),
        confidence=confidence,
        half_life_s=half_life_s,
    )
