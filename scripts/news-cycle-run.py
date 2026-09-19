# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Launcher for the NEWS aggregation cycle (``scripts/news-cycle.sh``
execs it through the venv interpreter).

Why a launcher and not ``python -m claude_worker.news cycle``: every
worker lane's overlap guard is ``pgrep -f 'claude[-_]worke[r]'`` (the
global worker-serialization law), which matches ANY cmdline carrying
``claude_worker``/``claude-worker`` — a module path, the package dir, the
venv path. This cycle runs for 25-31 s of every slot (it is
network-bound: 40-50 sources fetched in series), so a cmdline that
matched would make the 5-minute regime cycle skip roughly half its slots
and the hourly candles cycle skip whenever the two met. Its cmdline is
therefore ``<venv-alias>/bin/python3 <repo>/scripts/news-cycle-run.py``
— no such substring anywhere (the wrapper aliases the venv under
``~/multivenue/venv``, exactly as ``dashboard.sh`` does).

Being invisible to the guard is only safe because this lane SHARES NO
WRITER. The reason the law exists is ``state.db``'s single seq namespace,
and the news cycle never opens it: ``news.db`` is the only SQLite
connection anywhere in ``claude_worker.news`` (``news/store.py`` holds
the only ``sqlite3.connect``), the vocabulary reads JSON and TSV files,
and everything else it writes lives under ``~/multivenue/worker/news/``.
The asymmetry is deliberate and is the whole point: **this lane yields to
every other worker invocation and blocks none of them.** The wrapper
still checks the global guard before starting, and still refuses to
overlap a previous news cycle of its own.

Convention: full ``import x`` only. No ``from x import y``.
"""

import sys

import claude_worker.news.__main__

if __name__ == "__main__":
    sys.exit(claude_worker.news.__main__.main(["cycle"]))
