"""THE SDK seam (design §2/§5.2, §9.1 carried pattern): module-level
``make_client()`` + ``complete()`` — the ONLY place the Anthropic SDK is
touched, and the canonical monkeypatch point for every test.

Construction discipline: ``make_client`` takes a ``ServeConfig`` — the
sole holder of ``ANTHROPIC_API_KEY`` — and is called from ``daemon.py``
ONLY. Operator verbs never import-and-construct (asserted by the config
split: any verb must succeed with the key unset). Tests patch
``claude_worker.llm.make_client`` to return the conftest ``FakeClient``;
nothing below this seam runs in CI.

Convention: full ``import x`` only. No ``from x import y``.
"""

import typing

import anthropic

import claude_worker.config

# One response budget for triage/label calls (small structured JSON);
# the strategist (Fable 5, serve-only, future item) sets its own.
LLM_MAX_TOKENS: int = 1024


def make_client(cfg: claude_worker.config.ServeConfig) -> anthropic.Anthropic:
    """Construct the real SDK client. serve-only (§5.2); the API key
    never leaves the config object's repr-excluded field."""
    return anthropic.Anthropic(api_key=cfg.anthropic_api_key)


def complete(
    client: anthropic.Anthropic,
    model: str,
    prompt: str,
    max_tokens: int = LLM_MAX_TOKENS,
) -> str:
    """One user-turn completion; returns the first text block ("" when
    the model returned none — callers' strict parsers treat that as
    malformed and count it, per the §5.1 no-crash doctrine)."""
    message = client.messages.create(
        model=model,
        max_tokens=max_tokens,
        messages=[{"role": "user", "content": prompt}],
    )
    for block in message.content:
        if isinstance(block, anthropic.types.TextBlock):
            return block.text
    return ""


def complete_fn_for(client: anthropic.Anthropic) -> typing.Callable[[str, str], str]:
    """Adapt the client to the ``complete_fn(model, prompt)`` seam that
    feeds/labeling/state.cached_complete were built against (item 10)."""

    def fn(model: str, prompt: str) -> str:
        return complete(client, model, prompt)

    return fn
