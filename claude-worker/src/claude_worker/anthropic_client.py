"""Thin, mockable wrapper around the Anthropic SDK.

Why a wrapper? The codebase rule is full ``import x`` only — no
``from anthropic import Anthropic``. This wrapper is the single place that
touches the SDK, making it trivial to mock in tests by monkey-patching
``claude_worker.anthropic_client.make_client``.

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import typing

import anthropic


@dataclasses.dataclass(frozen=True, slots=True)
class CompletionRequest:
    """A minimal, typed request for a single Claude completion."""

    model: str
    system: str
    user: str
    max_tokens: int = 1024
    temperature: float = 0.0


@dataclasses.dataclass(frozen=True, slots=True)
class CompletionResponse:
    """A minimal, typed response."""

    text: str
    stop_reason: str
    input_tokens: int
    output_tokens: int


def make_client(api_key: str) -> anthropic.Anthropic:
    """Construct an Anthropic SDK client.

    Kept as a module-level function (not a method) so tests can monkey-patch
    it with ``monkeypatch.setattr("claude_worker.anthropic_client.make_client", ...)``.
    """
    return anthropic.Anthropic(api_key=api_key)


def complete(client: anthropic.Anthropic, req: CompletionRequest) -> CompletionResponse:
    """Run one completion synchronously.

    This is deliberately synchronous: the worker is offline, not hot path,
    and async adds complexity with no latency win here.
    """
    msg = client.messages.create(
        model=req.model,
        max_tokens=req.max_tokens,
        temperature=req.temperature,
        system=req.system,
        messages=[{"role": "user", "content": req.user}],
    )

    # The SDK returns ``content`` as a list of blocks; we only ever ask for text.
    parts: list[str] = []
    content_blocks = typing.cast(list[typing.Any], msg.content)
    for i in range(len(content_blocks)):
        block = content_blocks[i]
        block_type = getattr(block, "type", None)
        if block_type == "text":
            parts.append(typing.cast(str, getattr(block, "text", "")))

    usage = msg.usage
    return CompletionResponse(
        text="".join(parts),
        stop_reason=typing.cast(str, msg.stop_reason or ""),
        input_tokens=int(getattr(usage, "input_tokens", 0)),
        output_tokens=int(getattr(usage, "output_tokens", 0)),
    )
