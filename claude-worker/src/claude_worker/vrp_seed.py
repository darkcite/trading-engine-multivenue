# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""vrp-seed.tsv — the VRP member's boot seed (VRP V5).

The forecast (``core_vol`` / :mod:`claude_worker.vol_ref`) needs 60 settled
``(x, y)`` pairs before it will produce a bound, and the engine restarts
about three times a day. Without a seed the member would be blind for two
months. This lane cuts those pairs out of ``candles.db`` using the SAME
integer law the engine runs, so a seeded boot is arithmetically identical
to an engine that had been running since the first pair.

Rows are ``expiry_ts_ms<TAB>x_1e9<TAB>y_1e9``, oldest first. Read by
``core_config::vrp::parse_seed_row`` and replayed through
``VolEngine::seed_pair`` before ``on_start``.

**The lookahead law, which is the whole point of this file.** A pair is
``(x formed from the 1440 minutes BEFORE the entry instant, y realised
over the hold that followed)``. Two things follow, and both are enforced
here and tested:

1. A pair whose hold has NOT finished is never emitted. The cut-off is
   ``now``: an expiry at or after it has no realised ``y``, and inventing
   one is how a backtest of this strategy comes out profitable when the
   strategy is not.
2. ``x`` is formed by feeding the engine ONLY the minutes strictly before
   the entry instant. The engine is rebuilt per expiry from its own
   1440-minute window rather than run forward across expiries, so a
   minute inside one hold can never leak into the next one's regressor.

Lanes (``python -m claude_worker.vrp_seed <lane>``):

- ``seed-out --db <candles.db> --descriptor <d> --out <vrp-seed.tsv>``
  [``--tau-ns N``] [``--pairs N``] [``--now-ms N``] — the live boot file,
  written beside ``icdp.toml`` OUTSIDE git.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import dataclasses
import os
import pathlib
import sqlite3
import sys
import time
import tomllib
import typing

import claude_worker.vol_ref

#: Default output name, matching the engine's `--vrp-seed` default.
SEED_FILE: str = "vrp-seed.tsv"

#: Header line; the parser skips `#` lines.
#: Seed-file format version. 1 was bare ``expiry_ts_ms x_1e9 y_1e9``
#: triples; 2 is TAGGED and adds the ``R`` rows that carry the HAR's
#: rolling minute window. The bump is deliberate and one-way: a binary
#: older than 2026-09-11 refuses a v2 seed outright, which is what stops
#: it booting with a window it cannot account for. Deploy the binary
#: first, then re-cut the seed.
SEED_VERSION: int = 2

SEED_HEADER: str = (
    "# vrp-seed.tsv (VRP V5 + W4) - TAGGED rows, oldest first.\n"
    "#   V <version>\n"
    "#   P <expiry_ts_ms> <x_1e9> <y_1e9>   the fitted pairs\n"
    "#   R <min_ts_ms> <r_1e9>              the HAR's rolling window\n"
    "# Written by claude_worker.vrp_seed from candles.db, using the same\n"
    "# integer law core_vol runs. Never tracked by git.\n"
)

#: Expiries to emit by default. 60 is the engine's MIN_PAIRS, so the
#: default cut is the smallest one that lets a booted member decide.
PAIRS_DEFAULT: int = 90

#: Daily expiry instant: Deribit settles at 08:00 UTC.
EXPIRY_UTC_HOUR: int = 8

_MINUTE_MS: int = 60_000
_DAY_MS: int = 86_400_000
#: Two closes make one return; one close makes none.
_MIN_CLOSES_FOR_A_RETURN: int = 2

#: Minute returns to cut for the rolling window.
#:
#: ``core_vol::HAR_WARM_MINUTES`` is 1440 and N closes make N-1 returns,
#: so 1441 closes is the smallest cut that arrives WARM. We take a
#: little more than that: the engine's own state file covers the tail,
#: and a few spare minutes absorb a hole in candles.db without dropping
#: the whole window back under the gate.
WINDOW_MINUTES_DEFAULT: int = 1560


@dataclasses.dataclass(slots=True, frozen=True)
class SeedRow:
    """One settled expiry's fitted pair."""

    expiry_ts_ms: int
    x_1e9: int
    y_1e9: int


@dataclasses.dataclass(slots=True, frozen=True)
class WindowRow:
    """One minute's return for the HAR's rolling window.

    ``min_ts_ms`` is the OPEN of the minute that closed to produce this
    return, which is exactly what the engine stamps
    (``minute_id * 60_000``). The two sides must agree on this or the
    boot merge cannot line the series up.
    """

    min_ts_ms: int
    r_1e9: int


@dataclasses.dataclass(slots=True, frozen=True)
class CutSpec:
    """What to cut: the instrument, the tenor and how many pairs.

    One object rather than three parallel parameters, because these
    three always travel together and a caller that passes the tenor of
    one instrument with the descriptor of another has made a mistake
    nothing downstream can detect.
    """

    descriptor: str
    tau_ns: int = claude_worker.vol_ref.TAU_8H_NS
    pairs: int = PAIRS_DEFAULT
    window_minutes: int = WINDOW_MINUTES_DEFAULT


@dataclasses.dataclass(slots=True)
class CutStats:
    """What the cut saw, for the lane's one-line report."""

    expiries_considered: int = 0
    emitted: int = 0
    skipped_unsettled: int = 0
    skipped_short_history: int = 0
    skipped_short_hold: int = 0
    #: W4: minutes emitted for the rolling window.
    window_minutes: int = 0
    #: W4: holes in candles.db that truncated the window. A hole means
    #: the run before it is NOT contiguous with the run after, and a
    #: window that claims 24 h it does not have is worse than a short one.
    window_holes: int = 0


def closes_1e6(
    conn: sqlite3.Connection, descriptor: str, since_ms: int, until_ms: int
) -> list[tuple[int, int]]:
    """``(open_ts_ms, close_1e6)`` 1-minute closes in ``[since, until)``.

    Same read as :func:`claude_worker.regime.seed_rows_from_candles`: any
    source, NULL or non-positive closes skipped as holes the engine walks
    over.
    """
    cur = conn.execute(
        "SELECT open_ts, c FROM candles WHERE descriptor=? AND tf='1m'"
        " AND open_ts >= ? AND open_ts < ? ORDER BY open_ts",
        (descriptor, since_ms, until_ms),
    )
    out: list[tuple[int, int]] = []
    for open_ts, c in cur.fetchall():
        if c is None or c <= 0:
            continue
        close_1e6 = round(float(c) * 1_000_000)
        if close_1e6 > 0:
            out.append((open_ts, close_1e6))
    return out


def realised_rv_1e9(closes: list[tuple[int, int]]) -> int | None:
    """Realised vol over the given closes, raw bps x1e9.

    ``isqrt(sum r_k^2)`` over the hold's own returns — the same
    ``rv_W`` the HAR components are built from, which is what makes ``y``
    and ``x`` share a domain and lets the domain's constant offset cancel
    into the fitted intercept.
    """
    if len(closes) < _MIN_CLOSES_FOR_A_RETURN:
        return None
    acc = 0
    prev = closes[0][1]
    for _, close_1e6 in closes[1:]:
        r = claude_worker.vol_ref.ret_bps_1e9(prev, close_1e6)
        acc += r * r
        prev = close_1e6
    rv = claude_worker.vol_ref.isqrt_i64(acc)
    return rv if rv > 0 else None


def daily_expiries_ms(now_ms: int, count: int) -> list[int]:
    """The ``count`` most recent daily 08:00 UTC instants at or before
    ``now_ms``, oldest first."""
    day = (now_ms // _DAY_MS) * _DAY_MS
    latest = day + EXPIRY_UTC_HOUR * 3_600_000
    if latest > now_ms:
        latest -= _DAY_MS
    return [latest - i * _DAY_MS for i in range(count - 1, -1, -1)]


def pair_for_expiry(
    conn: sqlite3.Connection,
    spec: CutSpec,
    expiry_ts_ms: int,
    now_ms: int,
    stats: CutStats,
) -> SeedRow | None:
    """The ``(x, y)`` pair for one expiry, or ``None`` with a counted reason.

    ``x`` sees only ``[entry - 1440 min, entry)``; ``y`` sees only
    ``[entry, expiry]``. The two windows are disjoint by construction, so
    no minute of the hold can reach the regressor.
    """
    stats.expiries_considered += 1
    if expiry_ts_ms > now_ms:
        stats.skipped_unsettled += 1
        return None
    if claude_worker.vol_ref.tenor_of(spec.tau_ns) is None:
        raise ValueError(f"tau_ns {spec.tau_ns} is not a tradeable tenor")
    tau_ms = spec.tau_ns // 1_000_000
    entry_ms = expiry_ts_ms - tau_ms

    # `warm + 1` closes, not `warm`: the first close only seeds the
    # engine's `prev_px`, so N closes make N-1 returns and the 1440-minute
    # window needs 1441 of them to fill.
    warm = claude_worker.vol_ref.HAR_WINDOWS[-1]
    history = closes_1e6(conn, spec.descriptor, entry_ms - (warm + 1) * _MINUTE_MS, entry_ms)
    if len(history) < warm + 1:
        stats.skipped_short_history += 1
        return None
    engine = claude_worker.vol_ref.VolEngine()
    for _, close_1e6 in history:
        engine.on_minute_close(close_1e6)
    x = engine.x_1e9(spec.tau_ns)
    if x is None:
        stats.skipped_short_history += 1
        return None

    hold = closes_1e6(conn, spec.descriptor, entry_ms, expiry_ts_ms + _MINUTE_MS)
    rv = realised_rv_1e9(hold)
    if rv is None or len(hold) < tau_ms // _MINUTE_MS:
        stats.skipped_short_hold += 1
        return None
    y = claude_worker.vol_ref.ln_1e9(rv)
    if y == claude_worker.vol_ref.LOG2_UNDEFINED:
        stats.skipped_short_hold += 1
        return None
    stats.emitted += 1
    return SeedRow(expiry_ts_ms, x, y)


def window_returns(
    conn: sqlite3.Connection,
    descriptor: str,
    now_ms: int,
    minutes: int = WINDOW_MINUTES_DEFAULT,
    stats: CutStats | None = None,
) -> list[WindowRow]:
    """The trailing rolling window, oldest first and CONTIGUOUS.

    A return only exists between two adjacent minutes. Where candles.db
    has a hole the two closes either side are not adjacent, so the run
    breaks and everything before the newest break is dropped rather than
    spliced across the gap -- the engine's ``Sigma r^2`` is a sum over
    consecutive minutes, and a spliced return is not one of them.
    """
    since = now_ms - (minutes + 1) * _MINUTE_MS
    closes = closes_1e6(conn, descriptor, since, now_ms + _MINUTE_MS)
    if len(closes) < _MIN_CLOSES_FOR_A_RETURN:
        return []
    run: list[WindowRow] = []
    holes = 0
    i = 1
    while i < len(closes):
        prev_ts, prev_px = closes[i - 1]
        ts, px = closes[i]
        if ts - prev_ts != _MINUTE_MS:
            # Not adjacent: no return exists across the hole, and the
            # run so far can no longer reach the present contiguously.
            holes += 1
            run = []
        else:
            run.append(WindowRow(ts, claude_worker.vol_ref.ret_bps_1e9(prev_px, px)))
        i += 1
    if stats is not None:
        stats.window_holes = holes
        stats.window_minutes = len(run)
    return run


def cut_rows(
    db_path: pathlib.Path,
    spec: CutSpec,
    now_ms: int | None = None,
    lookback_days: int | None = None,
) -> tuple[list[SeedRow], CutStats]:
    """Cut up to ``spec.pairs`` settled pairs, oldest first."""
    now = int(time.time() * 1000) if now_ms is None else now_ms
    # Walk further back than `pairs` so holes in candles.db do not
    # silently shorten the seed.
    span = spec.pairs * 2 if lookback_days is None else lookback_days
    stats = CutStats()
    rows: list[SeedRow] = []
    conn = sqlite3.connect(db_path)
    try:
        for expiry in daily_expiries_ms(now, span):
            row = pair_for_expiry(conn, spec, expiry, now, stats)
            if row is not None:
                rows.append(row)
    finally:
        conn.close()
    return rows[-spec.pairs :], stats


def write_seed_tsv(
    path: pathlib.Path,
    rows: typing.Sequence[SeedRow],
    window: typing.Sequence[WindowRow] = (),
) -> None:
    """Atomic write (tmp + rename) so a boot never reads a torn file."""
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(SEED_HEADER)
        f.write(f"V\t{SEED_VERSION}\n")
        for r in rows:
            f.write(f"P\t{r.expiry_ts_ms}\t{r.x_1e9}\t{r.y_1e9}\n")
        for w in window:
            f.write(f"R\t{w.min_ts_ms}\t{w.r_1e9}\n")
    os.replace(tmp, path)


def seed_out(
    db_path: pathlib.Path,
    spec: CutSpec,
    out_path: pathlib.Path,
    now_ms: int | None = None,
) -> tuple[int, CutStats]:
    """The ``seed-out`` lane: returns ``(rows written, stats)``."""
    now = int(time.time() * 1000) if now_ms is None else now_ms
    rows, stats = cut_rows(db_path, spec, now)
    conn = sqlite3.connect(db_path)
    try:
        window = window_returns(conn, spec.descriptor, now, spec.window_minutes, stats)
    finally:
        conn.close()
    write_seed_tsv(out_path, rows, window)
    return len(rows), stats


def read_vrp_toml(path: pathlib.Path) -> tuple[str, int]:
    """``(underlying_descriptor, tau_ns)`` from ``vrp.toml``.

    Standard TOML here — the integer-only subset ``core_config::vrp``
    parses is a strict subset of it, so the worker can read the same file
    the engine boots from without owning a second grammar. Refuses a
    tenor the evidence does not support, exactly as the engine does.
    """
    obj = tomllib.loads(path.read_text(encoding="utf-8"))
    section = obj.get("vrp", {})
    descriptor = section.get("underlying_descriptor")
    tau_ns = section.get("tau_ns")
    if not isinstance(descriptor, str) or not descriptor:
        raise ValueError(f"{path}: missing `underlying_descriptor`")
    if not isinstance(tau_ns, int) or claude_worker.vol_ref.tenor_of(tau_ns) is None:
        raise ValueError(f"{path}: `tau_ns` {tau_ns!r} is not a tradeable tenor")
    return descriptor, tau_ns


def seed_for_window(
    run_dir: pathlib.Path,
    db_path: pathlib.Path,
    vrp_path: pathlib.Path,
    now_ms: int,
) -> tuple[int, CutStats]:
    """Write ``run_dir/vrp-seed.tsv`` for a harness window cut.

    Called from :func:`claude_worker.window_root.cut_run` beside the
    regime and funding seeds. ``now_ms`` is the WINDOW's first instant,
    not the wall clock: a window cut from last month must be seeded with
    what was known then, or the harness replays a forecast that had not
    been fitted yet. Nothing written when there is nothing to write.
    """
    descriptor, tau_ns = read_vrp_toml(vrp_path)
    spec = CutSpec(descriptor, tau_ns)
    rows, stats = cut_rows(db_path, spec, now_ms)
    conn = sqlite3.connect(db_path)
    try:
        window = window_returns(conn, descriptor, now_ms, spec.window_minutes, stats)
    finally:
        conn.close()
    if not rows and not window:
        return 0, stats
    write_seed_tsv(run_dir / SEED_FILE, rows, window)
    return len(rows), stats


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; the verb surface stays frozen once published."""
    ap = argparse.ArgumentParser(prog="claude_worker.vrp_seed")
    sub = ap.add_subparsers(dest="lane", required=True)
    out = sub.add_parser("seed-out", help="cut vrp-seed.tsv from candles.db")
    out.add_argument("--db", required=True, type=pathlib.Path)
    out.add_argument("--descriptor", required=True)
    out.add_argument("--out", required=True, type=pathlib.Path)
    out.add_argument("--tau-ns", type=int, default=claude_worker.vol_ref.TAU_8H_NS)
    out.add_argument("--pairs", type=int, default=PAIRS_DEFAULT)
    out.add_argument("--window-minutes", type=int, default=WINDOW_MINUTES_DEFAULT)
    out.add_argument("--now-ms", type=int, default=None)
    args = ap.parse_args(argv)

    spec = CutSpec(args.descriptor, args.tau_ns, args.pairs, args.window_minutes)
    n, stats = seed_out(args.db, spec, args.out, args.now_ms)
    print(
        f"vrp-seed: {n} pair(s) + {stats.window_minutes} window minute(s) -> {args.out} "
        f"(considered={stats.expiries_considered} unsettled={stats.skipped_unsettled} "
        f"short_history={stats.skipped_short_history} short_hold={stats.skipped_short_hold} "
        f"window_holes={stats.window_holes})"
    )
    if stats.window_minutes < claude_worker.vol_ref.HAR_WARM_MINUTES:
        print(
            f"vrp-seed: WARNING the window is {stats.window_minutes} minute(s); the "
            f"member needs {claude_worker.vol_ref.HAR_WARM_MINUTES} before it can "
            "forecast at all",
            file=sys.stderr,
        )
    if n < claude_worker.vol_ref.MIN_PAIRS:
        print(
            f"vrp-seed: WARNING only {n} pair(s); the member needs "
            f"{claude_worker.vol_ref.MIN_PAIRS} before it will decide",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
