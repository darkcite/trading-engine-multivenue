# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har_seed -- the long-tenor HAR over candles.db (HAR H2, H3.1).

Replays ``candles.db`` 1-minute closes of one series through
:class:`claude_worker.vol_ref.LongVolEngine` -- the bit-exact mirror of
``core_vol::LongVolEngine`` (pinned by ``tests/fixtures/vol/long-1.*``) --
so the numbers here are the numbers the engine will hold for the same
minutes. Three lanes (``python -m claude_worker.har_seed <lane>``; a
module, never a worker verb):

- ``show`` -- the forecast table per tenor: the raw fold and the rolling
  fit, annualised, the fitted line, the pair count and the QLIKE tell of
  raw against fit (plan law L2: a monthly number is a level, not a call --
  read the raw beside the fit).
- ``seed-out`` -- the boot seed for the engine's long-tenor state (the H3
  reader applies the rows in file order through ``seed_day`` /
  ``seed_open`` / ``seed_arm`` / ``seed_pair`` / ``seed_qlike`` and one
  ``refresh``; format below). Written atomically, OUTSIDE git.
- ``compare`` -- the per-day ``sum r^2`` agreement of two series over their
  overlap: the measurement a fallback must pass before its days are
  trusted (plan H2).

Each lane takes one series (``--descriptor D`` with ``--fallback F``,
repeatable) or every ``[[series]]`` of ``har.toml`` (``--har-toml P``,
``--series NAME`` to narrow): ``seed-out --har-toml`` writes
``seed-<NAME>.tsv`` per series into ``--out-dir`` (default
``~/multivenue/har``), and ``compare --har-toml`` measures each series'
adjacent sources (the feed against its first fallback, each fallback
against the next).

The LOOKAHEAD LAW: only minute bars CLOSED by ``--now-ms`` are read -- a
bar is stamped with its open and closes a minute later, and candles.db
upserts the still-open bar -- so the read stops at the minute ``now`` is
in. The replay starts ``--days`` UTC days before ``now`` (default 240: a
tenor's QLIKE window fills after 30 warm days, 60 pairs to fit and 60
scored settles -- 30 + 2 x 60 + 2 x tau, 230 for 40 d).

THE SPLICE: the fallbacks are ordered newest first, and each fills only the
minutes before the previous source's first minute (a source with no rows
in the window passes the same bound on to the next). The replay feeds the
oldest span first, and NO RETURN IS FORMED ACROSS A SOURCE BOUNDARY: the
first minute of each newer span only primes, as after an unobserved day.
Two venues' levels differ by their basis, and two sources of one
underlying may differ by a constant (the ``xyz`` index is 10.03 x SPY): a
return across the boundary would carry that level jump into the day's
``sum r^2``. The price is one return per boundary. ``compare`` measures
the sources' per-day agreement before a fallback is trusted.

Seed rows (v1), tab-separated, applied in file order (``#`` lines are
comments; the header names every span's source, oldest first):

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
import collections.abc
import dataclasses
import math
import os
import pathlib
import sqlite3
import statistics
import sys
import time
import typing

import claude_worker.candles
import claude_worker.har_config
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
#: Where ``seed-out --har-toml`` writes ``seed-<NAME>.tsv``.
OUT_DIR_DEFAULT: str = "~/multivenue/har"
_DAY_MS: int = claude_worker.vol_ref.DAY_MS
_MINUTE_MS: int = 60_000
_NONE: int = claude_worker.vol_ref.LOG2_UNDEFINED


@dataclasses.dataclass(frozen=True, slots=True)
class Span:
    """The minutes one source contributed to a replay."""

    descriptor: str
    minutes: int
    first_ts_ms: int
    last_ts_ms: int


@dataclasses.dataclass(slots=True)
class ReplayStats:
    """What the replay fed, for the lane's report line."""

    minutes: int = 0
    from_fallback: int = 0
    first_ts_ms: int = 0
    last_ts_ms: int = 0
    #: The sources that contributed, oldest first.
    spans: list[Span] = dataclasses.field(default_factory=list)
    #: Source boundaries crossed (each one primed, never returned across).
    splices: int = 0


def replay(
    conn: sqlite3.Connection,
    descriptor: str,
    now_ms: int,
    days: int = DAYS_DEFAULT,
    fallbacks: collections.abc.Sequence[str] = (),
) -> tuple[claude_worker.vol_ref.LongVolEngine, ReplayStats]:
    """The engine after every close of ``descriptor`` in the ``days`` UTC
    days before ``now_ms``, each of ``fallbacks`` filling in turn the
    minutes before the previous source's first one (module doc)."""
    since = (now_ms // _DAY_MS - days) * _DAY_MS
    # The lookahead law: the bar opened in the minute `now` is in has not
    # closed yet, so the read stops at that minute's open.
    until = now_ms - now_ms % _MINUTE_MS
    newest_first: list[tuple[str, list[tuple[int, int]]]] = []
    for source in (descriptor, *fallbacks):
        closes = claude_worker.vrp_seed.closes_1e6(conn, source, since, until)
        newest_first.append((source, closes))
        if closes:
            until = closes[0][0]
    engine = claude_worker.vol_ref.LongVolEngine()
    stats = ReplayStats()
    for source, closes in reversed(newest_first):
        if not closes:
            continue
        if stats.spans:
            # The splice law: the newer source's first minute only primes.
            engine.prev_px_1e6 = 0
            stats.splices += 1
        for ts, px in closes:
            engine.on_minute_close_at(px, ts)
        stats.spans.append(Span(source, len(closes), closes[0][0], closes[-1][0]))
        if source != descriptor:
            stats.from_fallback += len(closes)
        stats.minutes += len(closes)
    if stats.spans:
        stats.first_ts_ms = stats.spans[0].first_ts_ms
        stats.last_ts_ms = stats.spans[-1].last_ts_ms
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


def seed_header(label: str, descriptor: str, now_ms: int, stats: ReplayStats) -> str:
    """The comment lines a seed opens with: what it is, and every span."""
    lines = [
        f"# har-seed.tsv v{SEED_VERSION} (HAR H3) -- {label}: feed {descriptor}, now_ms={now_ms}.",
        "# Rows: see claude_worker.har_seed. Written from candles.db by the integer law",
        "# core_vol::LongVolEngine runs. Never tracked by git.",
        "# The sources, oldest first, one `span` line each: descriptor, first and last",
        "# minute (ms), minutes. No return is formed across a span boundary.",
    ]
    lines.extend(
        f"# span {s.descriptor} {s.first_ts_ms} {s.last_ts_ms} {s.minutes}" for s in stats.spans
    )
    return "\n".join(lines) + "\n"


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


def day_log_ratios(
    conn: sqlite3.Connection, a: str, b: str, since_ms: int, until_ms: int
) -> list[float]:
    """Per common full day, ``ln(vol_a / vol_b)`` = ``ln(sum_a / sum_b) / 2``,
    day order: the days both series cover at least
    ``COMPARE_MIN_MINUTES`` of."""
    sa = day_sums(claude_worker.vrp_seed.closes_1e6(conn, a, since_ms, until_ms))
    sb = day_sums(claude_worker.vrp_seed.closes_1e6(conn, b, since_ms, until_ms))
    return [
        math.log(sa[d][0] / sb[d][0]) / 2
        for d in sorted(sa.keys() & sb.keys())
        if min(sa[d][1], sb[d][1]) >= COMPARE_MIN_MINUTES and sa[d][0] > 0 and sb[d][0] > 0
    ]


def compare(
    conn: sqlite3.Connection, a: str, b: str, since_ms: int, until_ms: int
) -> tuple[int, float, float]:
    """``(days, median, p90)`` of the per-day ``|ln(vol_a / vol_b)|`` over
    the days both series cover at least ``COMPARE_MIN_MINUTES`` of."""
    ratios = sorted(abs(x) for x in day_log_ratios(conn, a, b, since_ms, until_ms))
    if not ratios:
        return (0, 0.0, 0.0)
    p90 = ratios[min(len(ratios) - 1, int(len(ratios) * 0.9))]
    return (len(ratios), statistics.median(ratios), p90)


def compare_line(
    conn: sqlite3.Connection, pair: tuple[str, str], window: tuple[int, int], label: str = ""
) -> str:
    """The lane's report line for one ``(a, b)`` pair over ``window =
    (since_ms, until_ms)``: the agreement, and the signed median -- the
    level offset of ``a`` over ``b`` (ruling 10 accepts it)."""
    a, b = pair
    signed = day_log_ratios(conn, a, b, *window)
    n, med, p90 = compare(conn, a, b, *window)
    offset = statistics.median(signed) if signed else 0.0
    return (
        f"har-seed compare {label}{a} vs {b}: {n} full day(s),"
        f" median |ln vol ratio| {med:.4f}, p90 {p90:.4f}, median ln vol ratio {offset:+.4f}"
    )


def connect_store(db: pathlib.Path) -> sqlite3.Connection:
    """The store for reading. A missing store is an error rather than a
    new empty file, and ``query_only`` refuses any write. Not ``mode=ro``:
    a read-only open of the live WAL store fails with SQLITE_CANTOPEN on
    the Mac (measured 2026-09-26), and ``immutable=1`` would skip the WAL
    -- the newest hour of candles."""
    if not db.is_file():
        raise FileNotFoundError(f"no candles store at {db}")
    conn = sqlite3.connect(db)
    conn.execute("PRAGMA query_only = 1")
    return conn


@dataclasses.dataclass(frozen=True, slots=True)
class Job:
    """One series to replay: its label, feed and ordered fallbacks."""

    label: str
    descriptor: str
    fallbacks: tuple[str, ...]


def _jobs(args: argparse.Namespace) -> list[Job]:
    if args.har_toml is None:
        return [Job(args.descriptor, args.descriptor, tuple(getattr(args, "fallback", None) or ()))]
    series = claude_worker.har_config.read(args.har_toml.expanduser())
    if args.series:
        wanted = set(args.series)
        series = [s for s in series if s.name in wanted]
    return [Job(s.name, s.feed, s.fallback) for s in series]


def _summary(job: Job, engine: claude_worker.vol_ref.LongVolEngine, stats: ReplayStats) -> str:
    spans = " + ".join(f"{s.descriptor} {s.minutes}" for s in stats.spans) or "no minutes"
    return (
        f"har-seed {job.label}: {stats.minutes} minute(s) ({spans}; {stats.splices} splice(s)),"
        f" {engine.n_resident()} day(s) resident, warm={engine.is_warm()},"
        f" gaps={engine.gaps} refused={engine.refused}"
    )


def _parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="claude_worker.har_seed")
    sub = ap.add_subparsers(dest="lane", required=True)
    for lane in ("show", "seed-out", "compare"):
        p = sub.add_parser(lane)
        p.add_argument("--db", default=None)
        which = p.add_mutually_exclusive_group(required=True)
        which.add_argument("--descriptor")
        which.add_argument("--har-toml", type=pathlib.Path)
        p.add_argument("--series", action="append", default=None, help="with --har-toml: only")
        p.add_argument("--days", type=int, default=DAYS_DEFAULT)
        p.add_argument("--now-ms", type=int, default=None)
        if lane == "show":
            p.add_argument("--tenors", default=",".join(str(t) for t in SHOW_TENORS))
        if lane != "compare":
            p.add_argument("--fallback", action="append", default=None, help="newest first")
        if lane == "seed-out":
            p.add_argument("--out", type=pathlib.Path, default=None)
            p.add_argument("--out-dir", type=pathlib.Path, default=None)
        if lane == "compare":
            p.add_argument("--against", default=None)
    return ap


def _refusal(args: argparse.Namespace) -> str | None:
    """What the parser cannot say: which flags go together."""
    if args.har_toml is not None and getattr(args, "fallback", None):
        return "--fallback goes with --descriptor; har.toml names each series' own"
    if args.lane == "seed-out" and args.descriptor is not None and args.out is None:
        return "seed-out --descriptor needs --out"
    if args.lane == "compare" and args.descriptor is not None and args.against is None:
        return "compare --descriptor needs --against"
    return None


def _compare_lane(conn: sqlite3.Connection, args: argparse.Namespace, jobs: list[Job]) -> int:
    now = args.now_ms
    window = ((now // _DAY_MS - args.days) * _DAY_MS, now)
    for job in jobs:
        if args.descriptor is not None:
            print(compare_line(conn, (job.descriptor, args.against), window))
            continue
        chain = (job.descriptor, *job.fallbacks)
        if len(chain) == 1:
            print(f"har-seed compare {job.label} {job.descriptor}: no fallback")
        for k in range(len(chain) - 1):
            print(compare_line(conn, (chain[k], chain[k + 1]), window, f"{job.label} "))
    return 0


def _seed_path(args: argparse.Namespace, job: Job) -> pathlib.Path:
    if args.descriptor is not None:
        return typing.cast(pathlib.Path, args.out)
    out_dir = (args.out_dir or pathlib.Path(OUT_DIR_DEFAULT)).expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)
    return out_dir / f"seed-{job.label}.tsv"


def _replay_lanes(conn: sqlite3.Connection, args: argparse.Namespace, jobs: list[Job]) -> int:
    """``show`` and ``seed-out``: one replay per job. A job with no minutes
    writes nothing (its last seed stands) and fails the run."""
    failed = 0
    for job in jobs:
        engine, stats = replay(conn, job.descriptor, args.now_ms, args.days, job.fallbacks)
        summary = _summary(job, engine, stats)
        if args.lane == "show":
            tenors = tuple(int(t) for t in args.tenors.split(","))
            print(summary)
            print("\n".join(tenor_report(engine, tenors)))
            continue
        if not stats.spans:
            print(f"{summary} -- nothing to write", file=sys.stderr)
            failed += 1
            continue
        out = _seed_path(args, job)
        header = seed_header(job.label, job.descriptor, args.now_ms, stats)
        write_seed(out, header, seed_rows(engine))
        print(f"{summary} -> {out}")
        if not engine.is_warm():
            print(f"har-seed: WARNING {job.label} is not warm (30 days)", file=sys.stderr)
    return 1 if failed else 0


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only -- never a worker verb)."""
    ap = _parser()
    args = ap.parse_args(argv)
    refusal = _refusal(args)
    if refusal is not None:
        ap.error(refusal)
    try:
        jobs = _jobs(args)
    except (OSError, claude_worker.har_config.HarConfigError) as e:
        print(f"har-seed: {args.har_toml}: {e}", file=sys.stderr)
        return 2
    db = pathlib.Path(
        args.db
        or os.environ.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH
    ).expanduser()
    if args.now_ms is None:
        args.now_ms = int(time.time() * 1000)
    try:
        conn = connect_store(db)
    except (OSError, sqlite3.Error) as e:
        print(f"har-seed: {e}", file=sys.stderr)
        return 2
    try:
        if args.lane == "compare":
            return _compare_lane(conn, args, jobs)
        return _replay_lanes(conn, args, jobs)
    finally:
        conn.close()


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
