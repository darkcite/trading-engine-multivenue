# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""K1.3 — importing the pinned upstream snapshot (plan §3a.3).

Upstream `shiyu-coder/Kronos` has no ``pyproject``/``setup.py``: it is a
top-level ``model`` package (``model/kronos.py`` does ``from model.module
import *``), so it cannot be a PyPI dependency and is not vendored into
this tree. :func:`activate` verifies the installed snapshot against
``kronos.lock``, puts its directory on ``sys.path``, and imports it.

Two refusals matter here:

* **A file whose hash does not match the lock.** The snapshot lives under
  ``~/multivenue/vendor/kronos/`` — outside git, writable, and never
  reviewed again after the install. Verifying at import is what makes
  "upstream as-is, pinned" true at the moment the code actually runs,
  not merely at the moment it was fetched.
* **A foreign ``model`` package already imported.** ``model`` is a
  spectacularly generic top-level name; if some other ``model`` module is
  in ``sys.modules`` we would silently forecast with it. The repo never
  reads ``KRONOS_ROOT`` and the research test bed (``~/Kronos_test``) is
  never on the path — the only ``model`` we accept is the one under our
  own vendor directory.

Convention: full ``import x`` only. No ``from x import y``.
"""

import importlib
import os
import pathlib
import sys
import types
import typing

import claude_worker.kronos.install


class UpstreamError(Exception):
    """The snapshot is missing, altered, or shadowed by another package."""


def vendor_dir(lock: dict[str, typing.Any]) -> pathlib.Path:
    """``~/multivenue/vendor/kronos/<upstream id>`` (see
    :func:`claude_worker.kronos.install.upstream_id`)."""
    return claude_worker.kronos.install.vendor_dir(lock)


def verify(lock: dict[str, typing.Any]) -> list[str]:
    """Snapshot files that are absent or do not match the lock."""
    upstream = typing.cast(dict[str, typing.Any], lock["upstream"])
    return claude_worker.kronos.install.check_installed(
        vendor_dir(lock), claude_worker.kronos.install._files_of(upstream)
    )


def _is_ours(module: types.ModuleType, directory: pathlib.Path) -> bool:
    path = getattr(module, "__file__", None)
    if not isinstance(path, str):
        # Namespace packages have no __file__; check their search paths.
        for entry in list(getattr(module, "__path__", []) or []):
            if os.path.commonpath([str(directory), str(entry)]) == str(directory):
                return True
        return False
    try:
        return os.path.commonpath([str(directory), path]) == str(directory)
    except ValueError:
        return False


def activate(lock: dict[str, typing.Any]) -> types.ModuleType:
    """Verify, path-inject, and import — returns ``model.kronos``.

    Idempotent: a second call re-uses the already-imported module (after
    checking it is still ours). Raises :class:`UpstreamError` rather than
    importing anything doubtful."""
    directory = vendor_dir(lock)
    if not directory.is_dir():
        raise UpstreamError(
            f"kronos: upstream snapshot not installed at {directory}"
            " — run `python -m claude_worker.kronos install`"
        )
    bad = verify(lock)
    if bad:
        raise UpstreamError(
            f"kronos: REFUSING to import {directory}: does not match kronos.lock: {', '.join(bad)}"
        )
    existing = sys.modules.get("model")
    if existing is not None and not _is_ours(existing, directory):
        raise UpstreamError(
            "kronos: a different top-level `model` package is already imported"
            f" ({getattr(existing, '__file__', '<namespace>')}) — refusing to shadow it"
        )
    text = str(directory)
    if text not in sys.path:
        sys.path.insert(0, text)
    try:
        return importlib.import_module("model.kronos")
    except ImportError as exc:  # torch/einops absent, or a broken snapshot
        raise UpstreamError(f"kronos: importing model.kronos failed: {exc}") from exc


def license_text(lock: dict[str, typing.Any]) -> str:
    """The snapshot's own LICENSE, as installed (MIT — the attribution
    the notices file points at)."""
    return (vendor_dir(lock) / "LICENSE").read_text(encoding="utf-8")
