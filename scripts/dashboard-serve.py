# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Launcher for the RG6 dashboard server (``scripts/dashboard.sh`` execs
it through the venv interpreter).

Why a launcher and not ``python -m claude_worker.dashboard``: every
worker lane's overlap guard is ``pgrep -f 'claude[-_]worke[r]'`` (the
global worker-serialization law), which matches ANY cmdline carrying
``claude_worker``/``claude-worker`` — a module path, the package dir,
the venv path. A long-running READ-ONLY server must never trip it
(the boot-time recommit would wait on it forever), so its cmdline is
``<venv-alias>/bin/python3 <repo>/scripts/dashboard-serve.py`` — no such
substring anywhere (the wrapper aliases the venv under
``~/multivenue/venv`` for the same reason).

Convention: full ``import x`` only. No ``from x import y``.
"""

import sys

import claude_worker.dashboard

if __name__ == "__main__":
    sys.exit(claude_worker.dashboard.main())
