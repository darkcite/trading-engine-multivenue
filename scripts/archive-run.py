#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Launcher for long archiver runs (the CMDLINE LAW, S-LAW 15).

Every worker lane's overlap guard is ``pgrep -f 'claude[-_]worke[r]'``, which
matches ANY command line carrying ``claude_worker`` or ``claude-worker`` —
``python -m claude_worker.archive``, the package directory, and the venv path
``claude-worker/.venv/bin/python3`` alike. The boot-time ruleset recommit waits
on that guard for five minutes and then gives up until the next boot, leaving
``vm_rows_active 0`` for hours.

So a long archiver run must not look like a worker. It runs as:

    ~/multivenue/venv/bin/python3 <repo>/scripts/archive-run.py <verb> …

where ``~/multivenue/venv`` is a directory symlink to the venv and this file
sits at the repo root — neither path contains the guard's pattern. Same shape as
``scripts/dashboard-serve.py``.

The archiver is genuinely outside the worker-serialization guard, not merely
hiding from it: it touches no ``state.db``, no ``ai.sock`` and no seq namespace.
It reads run directories and talks to the bucket. Its own single-instance guard
is ``pgrep -f 'archive-ru[n].py'``.

Short interactive commands may use the console script or
``uv run python -m claude_worker.archive`` instead — seconds are not a problem.
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "claude-worker" / "src"))

import claude_worker.archive

if __name__ == "__main__":
    sys.exit(claude_worker.archive.main())
