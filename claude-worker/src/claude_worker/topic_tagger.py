"""Bulk topic tagger — uses ``claude-haiku-4-5`` to label payloads.

Input:   iterable of ``(id, text)`` pairs.
Output:  list of ``TopicTag`` rows — deterministic, stable, JSON-serializable.

This is a **batch** job. It runs offline, writes artifacts to disk, and the
Rust engine consumes those artifacts at boot (never at runtime).

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import json
import pathlib
import typing

import claude_worker.anthropic_client
import claude_worker.config


_SYSTEM_PROMPT: str = (
    "You are a precise topic tagger for prediction-market news and tick data.\n"
    "Given a short text, return a JSON object with keys:\n"
    "  family: one of [crypto, politics, sports, macro, other]\n"
    "  impact: one of [low, med, high]\n"
    "  reason: short string (<=120 chars)\n"
    "Return ONLY the JSON object — no prose, no code fences."
)


@dataclasses.dataclass(frozen=True, slots=True)
class TopicTag:
    """One tagged record. Safe to JSON-serialize as-is."""

    id: str
    family: str
    impact: str
    reason: str


# Allowed vocabularies — used to validate model output cheaply.
_FAMILIES: frozenset[str] = frozenset(("crypto", "politics", "sports", "macro", "other"))
_IMPACTS: frozenset[str] = frozenset(("low", "med", "high"))


def _parse_tag(record_id: str, raw: str) -> TopicTag:
    """Parse a single JSON response. Falls back to ``other``/``low`` on malformed input."""
    try:
        obj = json.loads(raw)
    except json.JSONDecodeError:
        return TopicTag(id=record_id, family="other", impact="low", reason="malformed-json")

    if not isinstance(obj, dict):
        return TopicTag(id=record_id, family="other", impact="low", reason="not-an-object")

    family = str(obj.get("family", "other")).lower()
    if family not in _FAMILIES:
        family = "other"

    impact = str(obj.get("impact", "low")).lower()
    if impact not in _IMPACTS:
        impact = "low"

    reason = str(obj.get("reason", ""))[:120]

    return TopicTag(id=record_id, family=family, impact=impact, reason=reason)


def tag_batch(
    cfg: claude_worker.config.WorkerConfig,
    items: typing.Iterable[tuple[str, str]],
    *,
    model: str | None = None,
) -> list[TopicTag]:
    """Tag a batch of ``(id, text)`` items using the Haiku model.

    The client is constructed once per call. Tests can monkey-patch
    ``claude_worker.anthropic_client.make_client`` to inject a fake.
    """
    chosen_model: str = model if model is not None else claude_worker.config.MODEL_BULK
    client = claude_worker.anthropic_client.make_client(cfg.anthropic_api_key)

    out: list[TopicTag] = []
    for pair in items:
        record_id: str = pair[0]
        text: str = pair[1]

        req = claude_worker.anthropic_client.CompletionRequest(
            model=chosen_model,
            system=_SYSTEM_PROMPT,
            user=text,
            max_tokens=256,
            temperature=0.0,
        )
        resp = claude_worker.anthropic_client.complete(client, req)
        out.append(_parse_tag(record_id, resp.text.strip()))

    return out


def write_artifact(tags: typing.Sequence[TopicTag], path: pathlib.Path) -> None:
    """Write tags to ``path`` as newline-delimited JSON.

    The Rust side reads NDJSON (one object per line) at boot into a fixed-size
    symbol→tag table. Keep the schema stable.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for i in range(len(tags)):
            t = tags[i]
            obj: dict[str, str] = {
                "id": t.id,
                "family": t.family,
                "impact": t.impact,
                "reason": t.reason,
            }
            fh.write(json.dumps(obj, separators=(",", ":"), ensure_ascii=False))
            fh.write("\n")
