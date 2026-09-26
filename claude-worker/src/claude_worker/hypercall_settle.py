# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""hypercall_settle - the HC7 settlement shadow: the median-of-means law
replayed over the captured index, judged against the venue's posted S.

A standalone MODULE (``python -m claude_worker.hypercall_settle``) -
never a worker verb. For every (underlying, expiry) the research store
pins (``hypercall_history.settle_prices`` over ``hc_payouts``), it
gathers the captured index Marks (``hypercall-events.pmlr``: ``Mark`` on
the ``hypercall-idx:<U>`` sym, stamped with the entry's own source
time) across the capture root's run dirs, builds the 1 s
sample-and-hold grid over ``[T - 30 min, T]``, applies the law under
BOTH bucket orders and prints ``settle_err_bps`` per expiry - the
evidence the HC7 gate reads: median |err| <= 1 bp and max <= 3 bp over
>= 20 consecutive expiries on >= 3 underlyings, the bucket order chosen
by that data, before any member books with the law.

``grid`` and ``median_of_means`` are the Python MIRROR of
``core_settle`` (crates/core-settle), fixed point x1e6 exactly like the
Rust law. The pin table in tests/test_hypercall_settle.py matches the
Rust tests' - change either side only with the other in the same
commit.

Offline research tool: allocation is fine. Serialized like every worker
invocation (``pgrep -f claude-worker`` first).

Convention: full ``import x`` only.
"""

import argparse
import collections.abc
import datetime
import math
import os
import pathlib
import sqlite3
import statistics
import sys
import typing

import claude_worker.bin15_tape
import claude_worker.candles
import claude_worker.features
import claude_worker.hypercall_history
import claude_worker.pmlr

WINDOW_MS: int = 30 * 60 * 1000
STEP_MS: int = 1000
GRID_POINTS: int = WINDOW_MS // STEP_MS
TRIM_PER_MILLE: int = 50
ORDERS: tuple[str, ...] = ("sorted", "time")
EVENTS_FILE: str = "hypercall-events.pmlr"
LOG_DIR_ENV: str = "MULTIVENUE_LOG_DIR"
LOG_DIR_DEFAULT: str = "~/multivenue/logs"
#: The HC7 gate (plan section 3 HC7).
GATE_EXPIRIES: int = 20
GATE_UNDERLYINGS: int = 3
GATE_MEDIAN_BPS: float = 1.0
GATE_MAX_BPS: float = 3.0


def _trunc_div(a: int, b: int) -> int:
    """Integer division truncating toward zero (Rust ``/``)."""
    q = abs(a) // b
    return q if a >= 0 else -q


def median_of_means(samples: collections.abc.Sequence[int], order: str) -> int | None:
    """The law (``core_settle::median_of_means``): trim ``n * 50 // 1000``
    from each tail, ``k = isqrt(n')`` buckets of ``n' // k`` (the last
    takes the rest), the truncated mean of each, the median of the means
    (the truncated mean of the middle two when ``k`` is even)."""
    n = len(samples)
    if n == 0:
        return None
    c = n * TRIM_PER_MILLE // 1000
    ranked = sorted(samples)
    if order == "sorted":
        band = ranked[c : n - c]
    elif order == "time":
        lo, hi = ranked[c], ranked[n - c - 1]
        band = [x for x in samples if lo <= x <= hi]
    else:
        raise ValueError(f"unknown bucket order {order!r} (want one of {ORDERS})")
    n2 = len(band)
    k = max(1, math.isqrt(n2))
    size = n2 // k
    means = []
    for i in range(k):
        bucket = band[i * size : (i + 1) * size] if i < k - 1 else band[i * size :]
        means.append(_trunc_div(sum(bucket), len(bucket)))
    means.sort()
    mid = k // 2
    if k % 2 == 1:
        return means[mid]
    return _trunc_div(means[mid - 1] + means[mid], 2)


def grid(prints: collections.abc.Iterable[tuple[int, int]], t_end_ms: int) -> list[int]:
    """The 1 s sample-and-hold grid (``core_settle::SettleWindow``): grid
    instant ``T - 30 min + (i + 1) s`` holds the last price stamped at or
    before it; a price from before the window carries in; non-positive,
    out-of-order and after-expiry prints are refused (skipped)."""
    t0 = t_end_ms - WINDOW_MS
    out: list[int] = []
    nxt = 0
    last_px = 0
    last_ts = 0

    def advance(ts: int) -> None:
        nonlocal nxt
        while nxt < GRID_POINTS and t0 + (nxt + 1) * STEP_MS < ts:
            if last_px > 0:
                out.append(last_px)
            nxt += 1

    for ts, px in prints:
        if px <= 0 or ts < last_ts or ts > t_end_ms:
            continue
        advance(ts)
        last_px, last_ts = px, ts
    advance(t_end_ms + 1)
    return out


class Row(typing.NamedTuple):
    """One judged expiry."""

    underlying: str
    expiry_s: int
    points: int
    s_1e6: int
    consistent: bool
    mom_1e6: dict[str, int]
    err_bps: dict[str, float]


def index_marks(
    run_dirs: collections.abc.Iterable[pathlib.Path], underlying: str
) -> list[tuple[int, int]]:
    """Every captured index Mark of ``underlying`` - ``(source ms, px
    x1e6)`` - across ``run_dirs``, time-ordered."""
    want = f"hypercall-idx:{underlying}"
    out: list[tuple[int, int]] = []
    for run in run_dirs:
        syms = {s for s, d in claude_worker.bin15_tape.read_manifest(run).items() if d == want}
        path = run / EVENTS_FILE
        if not syms or not path.is_file():
            continue
        with claude_worker.pmlr.Reader(path) as reader:
            for ev in reader.channel_events():
                if ev.channel == claude_worker.pmlr.CHANNEL_MARK and ev.sym in syms:
                    out.append((ev.venue_time_ms, ev.v0))
    out.sort()
    return out


def shadow(
    conn: sqlite3.Connection,
    run_dirs: collections.abc.Sequence[pathlib.Path],
    only: collections.abc.Container[str] | None = None,
) -> list[Row]:
    """Judge every pinned expiry the capture covers (module doc)."""
    rows: list[Row] = []
    marks_by_und: dict[str, list[tuple[int, int]]] = {}
    for und, exp_s, s, _n, ok in claude_worker.hypercall_history.settle_prices(conn):
        if only is not None and und not in only:
            continue
        if und not in marks_by_und:
            marks_by_und[und] = index_marks(run_dirs, und)
        t_end = exp_s * 1000
        points = grid(
            (p for p in marks_by_und[und] if t_end - WINDOW_MS - 60_000 <= p[0] <= t_end), t_end
        )
        if not points:
            continue
        s_1e6 = round(s * 1_000_000)
        moms: dict[str, int] = {}
        errs: dict[str, float] = {}
        for order in ORDERS:
            m = median_of_means(points, order)
            if m is None:
                continue
            moms[order] = m
            errs[order] = (m - s_1e6) / s_1e6 * 1e4
        rows.append(Row(und, exp_s, len(points), s_1e6, ok, moms, errs))
    return rows


def gate(rows: collections.abc.Sequence[Row], order: str) -> tuple[bool, str]:
    """The HC7 gate over full-coverage, consistent rows for one order."""
    judged = [r for r in rows if r.consistent and r.points == GRID_POINTS and order in r.err_bps]
    errs = [abs(r.err_bps[order]) for r in judged]
    unds = {r.underlying for r in judged}
    if not errs:
        return False, f"{order}: no full-coverage expiry yet"
    med, worst = statistics.median(errs), max(errs)
    ok = (
        len(judged) >= GATE_EXPIRIES
        and len(unds) >= GATE_UNDERLYINGS
        and med <= GATE_MEDIAN_BPS
        and worst <= GATE_MAX_BPS
    )
    return ok, (
        f"{order}: {len(judged)} expiries on {len(unds)} underlyings,"
        f" median |err| {med:.3f} bp, max {worst:.3f} bp -> {'PASS' if ok else 'not yet'}"
    )


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only - never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.hypercall_settle")
    parser.add_argument("--db", default=None)
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument("--underlying", action="append", default=None)
    args = parser.parse_args(argv)
    env = os.environ
    db_path = pathlib.Path(
        args.db
        or env.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH
    ).expanduser()
    root = pathlib.Path(args.replay_dir or env.get(LOG_DIR_ENV, "") or LOG_DIR_DEFAULT).expanduser()
    conn = sqlite3.connect(db_path)
    try:
        claude_worker.hypercall_history.ensure_schema(conn)
        rows = shadow(conn, claude_worker.features.run_dirs(root), args.underlying)
    finally:
        conn.close()
    print("underlying\texpiry\tpoints\tS\tmom_sorted\terr_sorted_bp\tmom_time\terr_time_bp\tpins")
    for r in rows:
        day = datetime.datetime.fromtimestamp(r.expiry_s, tz=datetime.timezone.utc).isoformat()
        cells = [r.underlying, day, str(r.points), f"{r.s_1e6 / 1e6:.6f}"]
        for order in ORDERS:
            cells.append(f"{r.mom_1e6[order] / 1e6:.6f}" if order in r.mom_1e6 else "-")
            cells.append(f"{r.err_bps[order]:+.3f}" if order in r.err_bps else "-")
        cells.append("ok" if r.consistent else "INCONSISTENT")
        print("\t".join(cells))
    for order in ORDERS:
        print(gate(rows, order)[1], file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
