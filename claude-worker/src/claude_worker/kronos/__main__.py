# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""``python -m claude_worker.kronos <verb>`` — the package's CLI shim.

Module surface only: the worker's 8-verb console surface is FROZEN, so
every Kronos lane is reached this way (the `regime`/`candles` precedent).

Convention: full ``import x`` only. No ``from x import y``.
"""

import sys

import claude_worker.kronos.install

if __name__ == "__main__":
    sys.exit(claude_worker.kronos.install.main())
