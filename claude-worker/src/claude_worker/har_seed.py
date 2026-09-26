# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har_seed -- the long-tenor HAR over candles.db (HAR H2).

Replays ``candles.db`` 1-minute closes of one series through
:class:`claude_worker.vol_ref.LongVolEngine` -- the bit-exact mirror of
``core_vol::LongVolEngine`` (pinned by ``tests/fixtures/vol/long-1.*``) --
so the numbers here are the numbers the engine will hold for the same
minutes. Three lanes (``python -m claude_worker.har_seed <lane>``; a
module, never a worker verb):

- ``show --descriptor D`` -- the forecast table per tenor: the raw fold
  and the rolling fit, annualised, the fitted line, the pair count and
  the QLIKE tell of raw against fit (plan law L2: a monthly number is a
  level, not a call -- read the raw beside the fit).
- ``seed-out --descriptor D --out F`` -- the boot seed for the engine's
  long-tenor state (the H3 reader applies the rows in file order through
  ``seed_day`` / ``seed_open`` / ``seed_arm`` / ``seed_pair`` /
  ``seed_qlike`` and one ``refresh``; format below). Written atomically,
  OUTSIDE git, beside the other seeds.
- ``compare --descriptor A --against B`` -- the per-day ``sum r^2``
  agreement of two series over their overlap: the measurement a
  ``--fallback`` must pass before its days are trusted (plan H2).

The LOOKAHEAD LAW: only minute bars CLOSED by ``--now-ms`` are read — a
bar is stamped with its open and closes a minute later, and candles.db
upserts the still-open bar — so the read stops at the minute ``now`` is
in. The replay starts ``--days`` UTC days before ``now`` (default 240: a
tenor's QLIKE window fills after 30 warm days, 60 pairs to fit and 60
scored settles — 30 + 2 x 60 + 2 x tau, 230 for 40 d), and
``--fallback F`` fills only the minutes before the primary series' first
minute. The splice forms ONE return across the two series (the basis
between them lands in it, within the day's sum); ``compare`` measures the
two series' per-day agreement before a fallback is trusted.

Seed rows (v1), tab-separated, applied in file order:

    V 1
    D <day_ts_ms> <sum_sq> <n_min>                        closed days, oldest first
    C <day_ts_ms> <sum_sq> <n_min> <last_min_ts_ms> <prev_px_1e6>   the open day
    A <tau_days> <day_ts_ms> <x_1e9> <fit_1e9|->          the arms still pending
    P <tau_days> <target_day_ts_ms> <x_1e9> <y_1e9>       pairs, oldest first
    Q <tau_days> <raw_1e9> <fit_1e9>                      QLIKE rows, oldest first

Offline research tool: allocation and floats (in the reports only) are
fine; every number a seed row carries is the integer law's.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import dataclasses
import math
import os
import pathlib
import sqlite3
import statistics
import sys
import time

import claude_worker.candles
import claude_worker.vol_ref
import claude_worker.vrp_seed

SEED_VERSION: int = 1
#: UTC days replayed by default: warm (30) + pairs to fit (60) + a full
#: QLIKE window (60) + twice the longest target (80) + slack for holes —
#: the 1 M and 40 d QLIKE tells need 210 and 230.
DAYS_DEFAULT: int = 240
#: The plan's three named tenors (1 d / 1 w / 1 M), the ``show`` default.
SHOW_TENORS: tuple[int, ...] = (1, 7, 30)
#: A day both series cover at least this many minutes of is compared.
COMPARE_MIN_MINUTES: int = 1380
_DAY_MS: int = claude_worker.vol_ref.DAY_MS
_MINUTE_MS: int = 60_000
_NONE: int = claude_worker.vol_ref.LOG2_UNDEFINED


@dataclasses.dataclass(slots=True)
class ReplayStats:
    """What the replay fed, for the lane's report line."""

    minutes: int = 0
    from_fallback: int = 0
    first_ts_ms: int = 0
    last_ts_ms: int = 0


def replay(
    conn: sqlite3.Connection,
    descriptor: str,
    now_ms: int,
    days: int = DAYS_DEFAULT,
    fallback: str | None = None,
) -> tuple[claude_worker.vol_ref.LongVolEngine, ReplayStats]:
    """The engine after every close of ``descriptor`` in the ``days`` UTC
    days before ``now_ms`` (and of ``fallback`` before the primary's
    first minute), fed in time order."""
    since = (now_ms // _DAY_MS - days) * _DAY_MS
    # The lookahead law: the bar opened in the minute `now` is in has not
    # closed yet, so the read stops at that minute's open.
    closed_by = now_ms - now_ms % _MINUTE_MS
    closes = claude_worker.vrp_seed.closes_1e6(conn, descriptor, since, closed_by)
    stats = ReplayStats()
    if fallback is not None:
        until = closes[0][0] if closes else closed_by
        early = claude_worker.vrp_seed.closes_1e6(conn, fallback, since, until)
        stats.from_fallback = len(early)
        closes = early + closes
    engine = claude_worker.vol_ref.LongVolEngine()
    for ts, px in closes:
        engine.on_minute_close_at(px, ts)
    stats.minutes = len(closes)
    if closes:
        stats.first_ts_ms = closes[0][0]
        stats.last_ts_ms = closes[-1][0]
    return engine, stats


def seed_rows(engine: claude_worker.vol_ref.LongVolEngine) -> list[str]:
    """The v1 rows of ``engine``'s state (module doc): every resident day,
    the open day, the arms still pending, every pair and QLIKE row."""
    rows = [f"V\t{SEED_VERSION}"]
    n_res = engine.n_resident()
    for i in range(n_res):
        ts, sq, n = engine.day_at(i)
        rows.append(f"D\t{ts}\t{sq}\t{n}")
    open_day = engine.open_day()
    if open_day is not None:
        ts, sq, n = open_day
        rows.append(f"C\t{ts}\t{sq}\t{n}\t{engine.last_min_ts_ms}\t{engine.prev_px_1e6}")
    for d in range(1, claude_worker.vol_ref.LONG_TAU_DAYS_MAX + 1):
        tau = d * claude_worker.vol_ref.DAY_NS
        # Pending: armed at one of the last `d` closes, not yet settled.
        for i in range(max(0, n_res - d), n_res):
            arm = engine.arm_at(i, tau)
            if arm is not None:
                fit = "-" if arm[1] == _NONE else str(arm[1])
                rows.append(f"A\t{d}\t{engine.day_at(i)[0]}\t{arm[0]}\t{fit}")
    for d in range(1, claude_worker.vol_ref.LONG_TAU_DAYS_MAX + 1):
        tau = d * claude_worker.vol_ref.DAY_NS
        rows.extend(
            f"P\t{d}\t{p[0]}\t{p[1]}\t{p[2]}"
            for p in (engine.pair_at(tau, i) for i in range(engine.n_pairs(tau)))
        )
        rows.extend(
            f"Q\t{d}\t{q[0]}\t{q[1]}"
            for q in (engine.qlike_at(tau, i) for i in range(engine.qlike_counters(tau)[0]))
        )
    return rows


def write_seed(path: pathlib.Path, header: str, rows: list[str]) -> None:
    """Atomic write (tmp + rename): a boot never reads a torn seed."""
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(header + "\n".join(rows) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def _pct(v_1e9: int | None) -> str:
    return "-" if v_1e9 is None else f"{v_1e9 / 1e7:.2f}%"


def tenor_report(engine: claude_worker.vol_ref.LongVolEngine, tenors: tuple[int, ...]) -> list[str]:
    """One line per tenor: annualised raw and fitted sigma, the line, the
    pairs, and the QLIKE tell (lower is better)."""
    lines = ["tenor\tsigma_raw\tsigma_fit\ta\tb\tpairs\tqlike_n\tq_raw\tq_fit\tfit_beats_raw"]
    for d in tenors:
        tau = d * claude_worker.vol_ref.DAY_NS
        fit = engine.fit(tau)
        n, raw, fitq, beats = engine.qlike_counters(tau)
        lines.append(
            "\t".join(
                [
                    f"{d}d",
                    _pct(engine.sigma_ann_1e9(tau, "raw")),
                    _pct(engine.sigma_ann_1e9(tau, "fit")),
                    "-" if fit is None else f"{fit[0] / 1e9:.4f}",
                    "-" if fit is None else f"{fit[1] / 1e9:.4f}",
                    str(engine.n_pairs(tau)),
                    str(n),
                    f"{raw / 1e9:.4f}",
                    f"{fitq / 1e9:.4f}",
                    "yes" if beats else "no",
                ]
            )
        )
    return lines


def day_sums(closes: list[tuple[int, int]]) -> dict[int, tuple[int, int]]:
    """``{day_ts_ms: (sum_sq, n_returns)}`` -- the engine's per-day law
    (a return belongs to the day of its later minute)."""
    out: dict[int, tuple[int, int]] = {}
    prev = 0
    for ts, px in closes:
        if prev > 0:
            r = claude_worker.vol_ref.ret_bps_1e9(prev, px)
            day = ts - ts % _DAY_MS
            sq, n = out.get(day, (0, 0))
            out[day] = (sq + r * r, n + 1)
        prev = px
    return out


def compare(
    conn: sqlite3.Connection, a: str, b: str, since_ms: int, until_ms: int
) -> tuple[int, float, float]:
    """``(days, median, p90)`` of the per-day ``|ln(vol_a / vol_b)|`` over
    the days both series cover at least ``COMPARE_MIN_MINUTES`` of."""
    sa = day_sums(claude_worker.vrp_seed.closes_1e6(conn, a, since_ms, until_ms))
    sb = day_sums(claude_worker.vrp_seed.closes_1e6(conn, b, since_ms, until_ms))
    ratios = sorted(
        abs(math.log(sa[d][0] / sb[d][0])) / 2
        for d in sa.keys() & sb.keys()
        if min(sa[d][1], sb[d][1]) >= COMPARE_MIN_MINUTES and sa[d][0] > 0 and sb[d][0] > 0
    )
    if not ratios:
        return (0, 0.0, 0.0)
    p90 = ratios[min(len(ratios) - 1, int(len(ratios) * 0.9))]
    return (len(ratios), statistics.median(ratios), p90)


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only -- never a worker verb)."""
    ap = argparse.ArgumentParser(prog="claude_worker.har_seed")
    sub = ap.add_subparsers(dest="lane", required=True)
    for lane in ("show", "seed-out", "compare"):
        p = sub.add_parser(lane)
        p.add_argument("--db", default=None)
        p.add_argument("--descriptor", required=True)
        p.add_argument("--days", type=int, default=DAYS_DEFAULT)
        p.add_argument("--now-ms", type=int, default=None)
        if lane == "show":
            p.add_argument("--tenors", default=",".join(str(t) for t in SHOW_TENORS))
        if lane != "compare":
            p.add_argument("--fallback", default=None)
        if lane == "seed-out":
            p.add_argument("--out", required=True, type=pathlib.Path)
        if lane == "compare":
            p.add_argument("--against", required=True)
    args = ap.parse_args(argv)
    db = pathlib.Path(
        args.db
        or os.environ.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH
    ).expanduser()
    now = int(time.time() * 1000) if args.now_ms is None else args.now_ms
    conn = sqlite3.connect(db)
    try:
        if args.lane == "compare":
            since = (now // _DAY_MS - args.days) * _DAY_MS
            n, med, p90 = compare(conn, args.descriptor, args.against, since, now)
            print(
                f"har-seed compare {args.descriptor} vs {args.against}: {n} full day(s),"
                f" median |ln vol ratio| {med:.4f}, p90 {p90:.4f}"
            )
            return 0
        engine, stats = replay(conn, args.descriptor, now, args.days, args.fallback)
    finally:
        conn.close()
    summary = (
        f"har-seed {args.descriptor}: {stats.minutes} minute(s)"
        f" ({stats.from_fallback} from {args.fallback or 'no fallback'}),"
        f" {engine.n_resident()} day(s) resident, warm={engine.is_warm()},"
        f" gaps={engine.gaps} refused={engine.refused}"
    )
    if args.lane == "show":
        tenors = tuple(int(t) for t in args.tenors.split(","))
        print(summary)
        print("\n".join(tenor_report(engine, tenors)))
        return 0
    header = (
        f"# har-seed.tsv v{SEED_VERSION} (HAR H2) -- {args.descriptor}"
        f" (fallback {args.fallback or '-'}), now_ms={now}. Rows: see\n"
        "# claude_worker.har_seed. Written from candles.db by the integer law\n"
        "# core_vol::LongVolEngine runs. Never tracked by git.\n"
    )
    write_seed(args.out, header, seed_rows(engine))
    print(f"{summary} -> {args.out}")
    if not engine.is_warm():
        print("har-seed: WARNING the series is not warm (30 days)", file=sys.stderr)
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
