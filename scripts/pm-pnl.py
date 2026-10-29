# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Launcher for the PMJ P&L lane's pass (``scripts/pm-pnl.sh`` execs it
through the venv alias).

Why a launcher and not ``python -m claude_worker.pm_paper_pnl``: the
CMDLINE LAW (see ``scripts/dashboard-serve.py``) — every worker lane's
overlap guard is ``pgrep -f 'claude[-_]worke[r]'``, and this pass, which
runs every 15 minutes, must never hold another lane up.

Convention: full ``import x`` only. No ``from x import y``.
"""

import sys

import claude_worker.pm_paper_pnl

if __name__ == "__main__":
    sys.exit(claude_worker.pm_paper_pnl.main())
