# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""K1.3 — loading a pinned Kronos checkpoint (plan §3a.3).

The one behaviour this module exists for is **eval mode**. Upstream never
calls ``.eval()``: ``KronosPredictor`` constructs the modules, moves them
to the device and starts sampling with dropout still live. On the
research test bed that showed up twice (bed docs 01 §7, 04 §8) —
``Kronos-small`` hard-crashes on MPS without it, and on CPU every model
runs *silently* with dropout on, i.e. every forecast carries noise the
checkpoint was never meant to emit. So: ``.to(device)``, then
``.eval()`` on BOTH modules, then an assertion that neither is in
training mode. The assertion is not decoration — a future upstream
refactor that re-enters training mode would otherwise be invisible.

Weights are verified against ``kronos.lock`` before they are loaded: the
artifact directory is outside git and nothing else checks it at run time.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import typing

import claude_worker.kronos.install
import claude_worker.kronos.upstream


class WeightsError(Exception):
    """A checkpoint is missing, altered, or not named by the lock."""


def entry(lock: dict[str, typing.Any], name: str) -> dict[str, typing.Any]:
    weights = typing.cast(dict[str, typing.Any], lock["weights"])
    if name not in weights:
        raise WeightsError(f"kronos.lock has no weights entry {name!r}")
    return typing.cast(dict[str, typing.Any], weights[name])


def directory(name: str) -> pathlib.Path:
    return claude_worker.kronos.install.weights_dir(name)


def verify(lock: dict[str, typing.Any], name: str) -> list[str]:
    """Files of this checkpoint that are absent or off-hash."""
    return claude_worker.kronos.install.check_installed(
        directory(name), claude_worker.kronos.install._files_of(entry(lock, name))
    )


def require(lock: dict[str, typing.Any], name: str) -> pathlib.Path:
    path = directory(name)
    if not path.is_dir():
        raise WeightsError(
            f"kronos: {name} not installed at {path}"
            f" — run `python -m claude_worker.kronos install --model {name}`"
        )
    bad = verify(lock, name)
    if bad:
        raise WeightsError(
            f"kronos: REFUSING to load {name}: does not match kronos.lock: {', '.join(bad)}"
        )
    return path


def load(name: str, device: str, lock: dict[str, typing.Any]) -> tuple[typing.Any, typing.Any, int]:
    """``(tokenizer, model, max_context)`` on ``device``, both in eval
    mode. Verifies the snapshot and both checkpoints first."""
    model_entry = entry(lock, name)
    if str(model_entry.get("kind", "")) != "model":
        raise WeightsError(f"kronos: {name!r} is not a model entry")
    tokenizer_name = model_entry.get("tokenizer")
    if not isinstance(tokenizer_name, str) or not tokenizer_name:
        raise WeightsError(f"kronos.lock: [weights.{name!r}] names no tokenizer")
    max_context = model_entry.get("max_context")
    if not isinstance(max_context, int) or max_context <= 0:
        raise WeightsError(f"kronos.lock: [weights.{name!r}] has no usable max_context")

    kronos = claude_worker.kronos.upstream.activate(lock)
    model_dir = require(lock, name)
    tokenizer_dir = require(lock, tokenizer_name)

    tokenizer = kronos.KronosTokenizer.from_pretrained(str(tokenizer_dir))
    model = kronos.Kronos.from_pretrained(str(model_dir))
    tokenizer = tokenizer.to(device)
    model = model.to(device)
    # THE fix upstream lacks — see the module docstring.
    tokenizer.eval()
    model.eval()
    if getattr(tokenizer, "training", False) or getattr(model, "training", False):
        raise WeightsError("kronos: modules are still in training mode after .eval()")
    return tokenizer, model, max_context
