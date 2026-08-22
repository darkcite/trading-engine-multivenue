"""THE SDK seam (design §2/§5.2, §9.1 carried pattern): module-level
``make_client()`` + ``complete()`` — the ONLY place the Anthropic SDK is
touched, and the canonical monkeypatch point for every test.

Construction discipline: ``make_client`` takes a ``ServeConfig`` — the
sole holder of ``ANTHROPIC_API_KEY`` — and is called from ``daemon.py``
ONLY. Operator verbs never import-and-construct (asserted by the config
split: any verb must succeed with the key unset). Tests patch
``claude_worker.llm.make_client`` to return the conftest ``FakeClient``;
nothing below this seam runs in CI.

8h §7.2 surface growth (additive — existing triage/label callers are
byte-unchanged): [`complete_message`] carries an optional ``system``
block list (the strategist's static, ``cache_control: ephemeral`` prompt
prefix rides here) and returns a [`Completion`] with the ``message.usage``
token accounting the §7.5 budget ledger records. ``system`` is only
passed to the SDK when provided, so doubles programmed against the
pre-8h ``create(model=, max_tokens=, messages=)`` shape keep working.

Convention: full ``import x`` only. No ``from x import y``.
"""

import typing

import anthropic

import claude_worker.config

# One response budget for triage/label calls (small structured JSON).
LLM_MAX_TOKENS: int = 1024

# The strategist's own response budget (design §7.2; the "sets its own"
# note carried since 8f comes due here). Consumer: strategist.py, serve
# only, model `config.MODEL_STRATEGIST`.
STRATEGIST_MAX_TOKENS: int = 4096


class Completion(typing.NamedTuple):
    """One completion + its ``message.usage`` accounting (§7.5 ledger
    inputs). Doubles without a ``usage`` attribute yield zeros — the
    ledger then records an explicitly-zero row, never a crash."""

    text: str
    input_tokens: int
    output_tokens: int
    cache_read_input_tokens: int
    cache_creation_input_tokens: int


def make_client(cfg: claude_worker.config.ServeConfig) -> anthropic.Anthropic:
    """Construct the real SDK client. serve-only (§5.2); the API key
    never leaves the config object's repr-excluded field."""
    return anthropic.Anthropic(api_key=cfg.anthropic_api_key)


def _usage_int(usage: object, name: str) -> int:
    """Bool-rejecting, absent-tolerant usage field read (labeling.py
    numeric discipline; SDK doubles may carry no usage at all)."""
    value = getattr(usage, name, 0)
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        return 0
    return value


def complete_message(
    client: anthropic.Anthropic,
    model: str,
    prompt: str,
    *,
    max_tokens: int = LLM_MAX_TOKENS,
    system: list[dict[str, object]] | None = None,
) -> Completion:
    """One user-turn completion with the full §7.2 surface: optional
    ``system`` content blocks (each block dict may carry
    ``cache_control: {"type": "ephemeral"}`` for Anthropic prompt
    caching) and the usage numbers returned for the budget ledger.

    ``system`` is omitted from the SDK call when ``None`` — pre-8h call
    shapes (and their test doubles) are untouched by construction.
    """
    kwargs: dict[str, typing.Any] = {
        "model": model,
        "max_tokens": max_tokens,
        "messages": [{"role": "user", "content": prompt}],
    }
    if system is not None:
        kwargs["system"] = system
    message = client.messages.create(**kwargs)
    text = ""
    for block in message.content:
        if isinstance(block, anthropic.types.TextBlock):
            text = block.text
            break
    usage = getattr(message, "usage", None)
    return Completion(
        text=text,
        input_tokens=_usage_int(usage, "input_tokens"),
        output_tokens=_usage_int(usage, "output_tokens"),
        cache_read_input_tokens=_usage_int(usage, "cache_read_input_tokens"),
        cache_creation_input_tokens=_usage_int(usage, "cache_creation_input_tokens"),
    )


def complete(
    client: anthropic.Anthropic,
    model: str,
    prompt: str,
    max_tokens: int = LLM_MAX_TOKENS,
    system: list[dict[str, object]] | None = None,
) -> str:
    """One user-turn completion; returns the first text block ("" when
    the model returned none — callers' strict parsers treat that as
    malformed and count it, per the §5.1 no-crash doctrine)."""
    return complete_message(client, model, prompt, max_tokens=max_tokens, system=system).text


def complete_fn_for(client: anthropic.Anthropic) -> typing.Callable[[str, str], str]:
    """Adapt the client to the ``complete_fn(model, prompt)`` seam that
    feeds/labeling/state.cached_complete were built against (item 10)."""

    def fn(model: str, prompt: str) -> str:
        return complete(client, model, prompt)

    return fn
