"""Rule parser — uses ``claude-sonnet-4-6`` to turn a natural-language
research note into a structured ``StrategyRule``.

Output is written to ``artifacts_dir/rules/*.json`` and loaded by the Rust
engine at boot. Hot path is not involved.

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import json
import pathlib
import typing

import claude_worker.anthropic_client
import claude_worker.config


_SYSTEM_PROMPT: str = (
    "You are a strategy-rule extractor for a prediction-market trading system.\n"
    "Given a plain-English research note, return a JSON object with keys:\n"
    "  name:         short snake_case identifier\n"
    "  family:       one of [crypto, politics, sports, macro, other]\n"
    "  trigger:      short descriptor of the firing condition\n"
    "  edge_bps:     integer expected edge in basis points (>=0)\n"
    "  horizon_ms:   integer holding horizon in milliseconds (>0)\n"
    "  max_risk_usd: integer max position dollar risk (>0)\n"
    "Return ONLY the JSON object — no prose, no code fences."
)


@dataclasses.dataclass(frozen=True, slots=True)
class StrategyRule:
    """A single rule. Safe to JSON-serialize as-is."""

    name: str
    family: str
    trigger: str
    edge_bps: int
    horizon_ms: int
    max_risk_usd: int


class RuleParseError(ValueError):
    """Raised when the model response cannot be turned into a valid rule."""


_FAMILIES: frozenset[str] = frozenset(("crypto", "politics", "sports", "macro", "other"))


def _coerce_int(value: object, *, field: str, minimum: int) -> int:
    """Coerce a JSON value to an int and bounds-check it."""
    if isinstance(value, bool):
        # bool is a subclass of int in Python — reject explicitly.
        raise RuleParseError(f"field {field!r}: bool is not a valid integer")
    if not isinstance(value, (int, float)):
        raise RuleParseError(f"field {field!r}: expected number, got {type(value).__name__}")
    coerced = int(value)
    if coerced < minimum:
        raise RuleParseError(f"field {field!r}: {coerced} < minimum {minimum}")
    return coerced


def _parse_rule(raw: str) -> StrategyRule:
    """Parse a model response into a ``StrategyRule`` — raises on bad input."""
    try:
        obj = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuleParseError(f"response is not valid JSON: {exc}") from exc

    if not isinstance(obj, dict):
        raise RuleParseError(f"response is not a JSON object: {type(obj).__name__}")

    name = str(obj.get("name", "")).strip()
    if not name:
        raise RuleParseError("field 'name' is empty")

    family = str(obj.get("family", "")).lower()
    if family not in _FAMILIES:
        raise RuleParseError(f"field 'family': {family!r} not in {sorted(_FAMILIES)}")

    trigger = str(obj.get("trigger", "")).strip()
    if not trigger:
        raise RuleParseError("field 'trigger' is empty")

    edge_bps = _coerce_int(obj.get("edge_bps", -1), field="edge_bps", minimum=0)
    horizon_ms = _coerce_int(obj.get("horizon_ms", 0), field="horizon_ms", minimum=1)
    max_risk_usd = _coerce_int(obj.get("max_risk_usd", 0), field="max_risk_usd", minimum=1)

    return StrategyRule(
        name=name,
        family=family,
        trigger=trigger,
        edge_bps=edge_bps,
        horizon_ms=horizon_ms,
        max_risk_usd=max_risk_usd,
    )


def parse_note(
    cfg: claude_worker.config.WorkerConfig,
    note: str,
    *,
    model: str | None = None,
) -> StrategyRule:
    """Parse a single research note into a ``StrategyRule``."""
    chosen_model: str = model if model is not None else claude_worker.config.MODEL_REASONING
    client = claude_worker.anthropic_client.make_client(cfg.anthropic_api_key)

    req = claude_worker.anthropic_client.CompletionRequest(
        model=chosen_model,
        system=_SYSTEM_PROMPT,
        user=note,
        max_tokens=512,
        temperature=0.0,
    )
    resp = claude_worker.anthropic_client.complete(client, req)
    return _parse_rule(resp.text.strip())


def write_artifact(rules: typing.Sequence[StrategyRule], path: pathlib.Path) -> None:
    """Write rules to ``path`` as a single JSON array.

    The Rust engine reads this into a fixed-size rules table at boot.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: list[dict[str, object]] = []
    for i in range(len(rules)):
        r = rules[i]
        payload.append(
            {
                "name": r.name,
                "family": r.family,
                "trigger": r.trigger,
                "edge_bps": r.edge_bps,
                "horizon_ms": r.horizon_ms,
                "max_risk_usd": r.max_risk_usd,
            }
        )
    with path.open("w", encoding="utf-8") as fh:
        json.dump(payload, fh, separators=(",", ":"), ensure_ascii=False)
