# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15-seed-<COIN>.tsv -- the BIN15 member's boot seed (O4b, spec §6.2).

The member prices off ``core-vol``, which needs 1 440 minutes of
returns before it forecasts at all and 60 settled ``(x, y)`` pairs
before it fits a line. The engine restarts about three times a UTC day
(00:10 / 08:30 / 16:05Z), and a quarter-hour binary's own pairs accrue
at four an hour, so an unseeded member spends most of its life unable
to price. This lane cuts both out of ``candles.db`` with the SAME
integer law the engine runs, so a seeded boot is arithmetically
identical to an engine that had been up since the first pair.

**The grammar is the VRP seed grammar, deliberately** -- ``V 2`` then
``P expiry_ts_ms x_1e9 y_1e9`` then ``R min_ts_ms r_1e9``, written by
:func:`claude_worker.vrp_seed.write_seed_tsv` itself rather than by a
second copy of it, and read back by ``core_config::vrp::parse_returns``
/ ``parse_pairs``, which is what ``crates/cli/src/bin15_boot.rs``
already calls. Two implementations of one file format is how the two
drift.

**A pair belongs to ONE tenor, which is why there are two files.**
``x`` is ``ln har_tau`` and ``y`` is ``ln`` realised vol over that same
``tau``; the 15 m tenor folds four HAR windows and the 8 h tenor folds
three, so a 15-minute pair is not an 8-hour pair and fitting one line
through both is fitting the wrong regression. Ruling O-Q7 puts both
kinds of family on one underlying, so each underlying gets:

* ``bin15-seed-<COIN>.tsv`` -- the 15 m tenor's pairs on a quarter-hour
  expiry grid, PLUS the shared rolling minute window. The
  ``out:<COIN>:15m`` families read it.
* ``bin15-seed-<COIN>-1d.tsv`` -- the 8 h tenor's pairs on the daily
  06:00 UTC expiry grid. The ``native:<COIN>:1d`` families read it.
  Pairs only: the minute window is a property of the price series, not
  of the horizon, so it lives in the first file and boot pushes it to
  both forecast engines.

Both are OPTIONAL at boot. An absent file is a cold tenor that holds
until its window warms, which is a legal state the engine must survive;
a malformed one refuses the boot.

**The lookahead law, which is the whole point of this file.** A pair is
``(x formed from the 1 440 minutes strictly BEFORE the entry instant, y
realised over the hold that followed)``:

1. A pair whose hold has not finished is never emitted. The cut-off is
   ``now`` -- an expiry at or after it has no realised ``y``, and
   inventing one is how a backtest of this member comes out profitable
   when the member is not.
2. The forecast engine is rebuilt per expiry from its own 1 440-minute
   window rather than run forward across expiries, so a minute inside
   one hold can never reach the next one's regressor.

**The mark source.** The member prices off the venue's own perp mark
(``hyperliquid:<COIN>``), and ``candles.db`` carries that descriptor, so
it is the default here and the seed is cut from the same series the
member will see live. ``--descriptor`` overrides it -- ``binance-usdm:<coin>usdt``
is the documented fallback for a coin whose HL history is too short to
fill the window. Which one was used is the O5 ledger's business to
reconcile; this lane only records it.

Lanes (``python -m claude_worker.bin15_seed <lane>``):

- ``seed-out --db <candles.db> --family <key> --out <path>``
  [``--descriptor d``] [``--pairs N``] [``--now-ms N``]
  [``--window`` | ``--no-window``] -- one file.
- ``seed-all --db <candles.db> --bin15 <bin15.toml> --dir <dir>``
  [``--pairs N``] [``--now-ms N``] -- every file the artifact's
  ``underlying`` list implies, named the way ``bin15_boot`` looks them
  up. This is what the engine wrapper calls.

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
import claude_worker.vrp_seed

#: The live candles database. `~/multivenue/candles.db` is a 0-byte
#: stray; the real one is under `worker/`, and the same constant in
#: `xsd_author` is the precedent — one spelling, not one per caller.
DEFAULT_DB: str = "~/multivenue/worker/candles.db"

#: Pairs to emit by default. ``core_vol``'s pair ring is 128 and the
#: spec asks for 128, so the default cut fills it exactly.
PAIRS_DEFAULT: int = 128

#: Minute returns to cut for the rolling window. ``HAR_WARM_MINUTES`` is
#: 1 440 and N closes make N-1 returns, so 1 441 closes is the smallest
#: cut that arrives WARM; the spare minutes absorb a hole in candles.db
#: without dropping the window back under the gate. Same number and
#: same reasoning as ``vrp_seed.WINDOW_MINUTES_DEFAULT``.
WINDOW_MINUTES_DEFAULT: int = 1_560

#: The native-daily families' expiry instant (spec §2: ``period:1d``
#: settles 06:00 UTC). NOT the VRP lane's 08:00 -- that is Deribit's.
NATIVE_EXPIRY_UTC_HOUR: int = 6

#: The 15 m families' expiry grid: every quarter-hour, on the quarter.
QUARTER_MS: int = 900_000

#: Family kinds, mirroring ``strategy_bin15::{FAMILY_OUT_15M, FAMILY_NATIVE_DAILY}``.
FAMILY_OUT_15M: int = 0
FAMILY_NATIVE_DAILY: int = 1

#: The suffix ``bin15_boot`` appends for the daily tenor's pair file.
#: Changing it changes where the engine looks.
DAILY_SUFFIX: str = "-1d"

_MINUTE_MS: int = 60_000
_DAY_MS: int = 86_400_000


@dataclasses.dataclass(slots=True, frozen=True)
class Family:
    """One ``bin15.toml`` family key, taken apart.

    One object rather than three parallel parameters, because the coin,
    the kind and the tenor always travel together and a caller that
    passes one family's tenor with another's coin has made a mistake
    nothing downstream can detect.
    """

    key: str
    coin: str
    kind: int

    @property
    def tau_ns(self) -> int:
        """The ``core-vol`` tenor this kind prices on
        (``strategy_bin15::tau_of_kind``)."""
        if self.kind == FAMILY_NATIVE_DAILY:
            return claude_worker.vol_ref.TAU_8H_NS
        return claude_worker.vol_ref.TAU_15M_NS

    @property
    def default_descriptor(self) -> str:
        """The mark source the member itself uses."""
        return f"hyperliquid:{self.coin}"

    def seed_name(self) -> str:
        """The file name ``bin15_boot`` looks this tenor up under."""
        suffix = DAILY_SUFFIX if self.kind == FAMILY_NATIVE_DAILY else ""
        return f"bin15-seed-{self.coin}{suffix}.tsv"


def parse_family(key: str) -> Family:
    """``out:<COIN>:15m`` or ``native:<COIN>:1d``, or a refusal.

    The same two forms ``bin15_boot::kind_of_key`` accepts, and crossed
    forms (``out:BTC:1d``) are refused on both sides -- a crossed key is
    how a daily comes to be seeded off a 15-minute forecast.
    """
    parts = key.split(":")
    if len(parts) != 3:
        raise ValueError(f"`{key}` is not a family key (want `out:<COIN>:15m` or `native:<COIN>:1d`)")
    head, coin, period = parts
    if not coin:
        raise ValueError(f"`{key}` names no coin")
    if (head, period) == ("out", "15m"):
        return Family(key, coin, FAMILY_OUT_15M)
    if (head, period) == ("native", "1d"):
        return Family(key, coin, FAMILY_NATIVE_DAILY)
    raise ValueError(f"`{key}` is not a family key; crossed forms are refused")


@dataclasses.dataclass(slots=True, frozen=True)
class CutSpec:
    """What to cut: the family, the price series and how many pairs."""

    family: Family
    descriptor: str
    pairs: int = PAIRS_DEFAULT
    window_minutes: int = WINDOW_MINUTES_DEFAULT


def quarter_hour_expiries_ms(now_ms: int, count: int) -> list[int]:
    """The ``count`` most recent quarter-hour instants at or before
    ``now_ms``, oldest first.

    A 15 m instance is created at the previous one's expiry second and
    expires on the quarter (spec §2), so the quarter-hour grid IS the
    expiry grid -- no instance list is needed to reconstruct it.
    """
    latest = (now_ms // QUARTER_MS) * QUARTER_MS
    return [latest - i * QUARTER_MS for i in range(count - 1, -1, -1)]


def native_expiries_ms(now_ms: int, count: int) -> list[int]:
    """The ``count`` most recent 06:00 UTC instants at or before
    ``now_ms``, oldest first."""
    day = (now_ms // _DAY_MS) * _DAY_MS
    latest = day + NATIVE_EXPIRY_UTC_HOUR * 3_600_000
    if latest > now_ms:
        latest -= _DAY_MS
    return [latest - i * _DAY_MS for i in range(count - 1, -1, -1)]


def expiries_ms(family: Family, now_ms: int, count: int) -> list[int]:
    """The expiry grid this family settles on."""
    if family.kind == FAMILY_NATIVE_DAILY:
        return native_expiries_ms(now_ms, count)
    return quarter_hour_expiries_ms(now_ms, count)


def pair_for_expiry(
    conn: sqlite3.Connection,
    spec: CutSpec,
    expiry_ts_ms: int,
    now_ms: int,
    stats: claude_worker.vrp_seed.CutStats,
) -> claude_worker.vrp_seed.SeedRow | None:
    """The ``(x, y)`` pair for one expiry, or ``None`` with a counted reason.

    ``x`` sees only ``[entry - 1440 min, entry)``; ``y`` sees only
    ``[entry, expiry]``. The two windows are disjoint by construction,
    so no minute of the hold can reach the regressor.
    """
    stats.expiries_considered += 1
    if expiry_ts_ms > now_ms:
        stats.skipped_unsettled += 1
        return None
    tau_ns = spec.family.tau_ns
    if claude_worker.vol_ref.tenor_of(tau_ns) is None:
        raise ValueError(f"tau_ns {tau_ns} is not a tenor core-vol forecasts")
    tau_ms = tau_ns // 1_000_000
    entry_ms = expiry_ts_ms - tau_ms

    # `warm + 1` closes, not `warm`: the first close only seeds the
    # engine's `prev_px`, so N closes make N-1 returns.
    warm = claude_worker.vol_ref.HAR_WARM_MINUTES
    history = claude_worker.vrp_seed.closes_1e6(
        conn, spec.descriptor, entry_ms - (warm + 1) * _MINUTE_MS, entry_ms
    )
    if len(history) < warm + 1:
        stats.skipped_short_history += 1
        return None
    engine = claude_worker.vol_ref.VolEngine()
    for _, close_1e6 in history:
        engine.on_minute_close(close_1e6)
    x = engine.x_1e9(tau_ns)
    if x is None:
        stats.skipped_short_history += 1
        return None

    hold = claude_worker.vrp_seed.closes_1e6(
        conn, spec.descriptor, entry_ms, expiry_ts_ms + _MINUTE_MS
    )
    rv = claude_worker.vrp_seed.realised_rv_1e9(hold)
    if rv is None or len(hold) < tau_ms // _MINUTE_MS:
        stats.skipped_short_hold += 1
        return None
    y = claude_worker.vol_ref.ln_1e9(rv)
    if y == claude_worker.vol_ref.LOG2_UNDEFINED:
        stats.skipped_short_hold += 1
        return None
    stats.emitted += 1
    return claude_worker.vrp_seed.SeedRow(expiry_ts_ms, x, y)


def cut_rows(
    db_path: pathlib.Path,
    spec: CutSpec,
    now_ms: int | None = None,
    lookback: int | None = None,
) -> tuple[list[claude_worker.vrp_seed.SeedRow], claude_worker.vrp_seed.CutStats]:
    """Cut up to ``spec.pairs`` settled pairs, oldest first."""
    now = int(time.time() * 1000) if now_ms is None else now_ms
    # Walk further back than `pairs` so holes in candles.db do not
    # silently shorten the seed.
    span = spec.pairs * 2 if lookback is None else lookback
    stats = claude_worker.vrp_seed.CutStats()
    rows: list[claude_worker.vrp_seed.SeedRow] = []
    conn = sqlite3.connect(db_path)
    try:
        for expiry in expiries_ms(spec.family, now, span):
            row = pair_for_expiry(conn, spec, expiry, now, stats)
            if row is not None:
                rows.append(row)
    finally:
        conn.close()
    return rows[-spec.pairs :], stats


def seed_out(
    db_path: pathlib.Path,
    spec: CutSpec,
    out_path: pathlib.Path,
    now_ms: int | None = None,
    window: bool | None = None,
) -> tuple[int, claude_worker.vrp_seed.CutStats]:
    """The ``seed-out`` lane: returns ``(pairs written, stats)``.

    ``window`` defaults to ON for the 15 m file and OFF for the daily
    one, because the minute window is a property of the price series
    rather than of the horizon and boot pushes the first file's rows to
    both forecast engines. Carrying it twice would only make the daily
    file 1 560 lines longer.
    """
    now = int(time.time() * 1000) if now_ms is None else now_ms
    want_window = spec.family.kind == FAMILY_OUT_15M if window is None else window
    rows, stats = cut_rows(db_path, spec, now)
    minutes: typing.Sequence[claude_worker.vrp_seed.WindowRow] = ()
    if want_window:
        conn = sqlite3.connect(db_path)
        try:
            minutes = claude_worker.vrp_seed.window_returns(
                conn, spec.descriptor, now, spec.window_minutes, stats
            )
        finally:
            conn.close()
    # The VRP lane's writer, not a second copy of it: one grammar, one
    # atomic write (tmp + rename), so a boot never reads a torn file.
    claude_worker.vrp_seed.write_seed_tsv(out_path, rows, minutes)
    return len(rows), stats


def read_bin15_toml(path: pathlib.Path) -> tuple[list[str], list[str]]:
    """``(families, underlying)`` from ``bin15.toml``.

    Standard TOML here -- the integer-only subset ``core_config::bin15``
    parses is a strict subset of it, so the worker reads the same file
    the engine boots from without owning a second grammar.
    """
    obj = tomllib.loads(path.read_text(encoding="utf-8"))
    section = obj.get("bin15", {})
    families = section.get("families")
    underlying = section.get("underlying")
    if not isinstance(families, list) or not families:
        raise ValueError(f"{path}: missing `families`")
    if not isinstance(underlying, list) or not underlying:
        raise ValueError(f"{path}: missing `underlying`")
    return [str(f) for f in families], [str(u) for u in underlying]


def seed_all(
    db_path: pathlib.Path,
    bin15_path: pathlib.Path,
    out_dir: pathlib.Path,
    pairs: int = PAIRS_DEFAULT,
    now_ms: int | None = None,
) -> list[tuple[str, int, claude_worker.vrp_seed.CutStats]]:
    """Every seed file the artifact implies, in family order.

    Returns one ``(file name, pairs, stats)`` row per file written. A
    family whose kind has no coin in ``underlying`` is skipped and
    reported -- ``bin15_boot`` would refuse that artifact anyway, and
    this lane is not the place to enforce it twice.
    """
    families, underlying = read_bin15_toml(bin15_path)
    coins = {u.rsplit(":", 1)[-1] for u in underlying}
    out: list[tuple[str, int, claude_worker.vrp_seed.CutStats]] = []
    seen: set[str] = set()
    for key in families:
        fam = parse_family(key)
        if fam.coin not in coins:
            continue
        name = fam.seed_name()
        if name in seen:
            continue  # two families of one kind share a coin's file
        seen.add(name)
        spec = CutSpec(fam, fam.default_descriptor, pairs)
        n, stats = seed_out(db_path, spec, out_dir / name, now_ms)
        out.append((name, n, stats))
    return out


def _report(name: str, n: int, stats: claude_worker.vrp_seed.CutStats) -> str:
    return (
        f"bin15-seed: {n} pair(s) + {stats.window_minutes} window minute(s) -> {name} "
        f"(considered={stats.expiries_considered} unsettled={stats.skipped_unsettled} "
        f"short_history={stats.skipped_short_history} short_hold={stats.skipped_short_hold} "
        f"window_holes={stats.window_holes})"
    )


def _warn(n: int, stats: claude_worker.vrp_seed.CutStats, window: bool) -> None:
    if window and stats.window_minutes < claude_worker.vol_ref.HAR_WARM_MINUTES:
        print(
            f"bin15-seed: WARNING the window is {stats.window_minutes} minute(s); the "
            f"member needs {claude_worker.vol_ref.HAR_WARM_MINUTES} before it can "
            "forecast at all",
            file=sys.stderr,
        )
    if n < claude_worker.vol_ref.MIN_PAIRS:
        print(
            f"bin15-seed: WARNING only {n} pair(s); the forecast needs "
            f"{claude_worker.vol_ref.MIN_PAIRS} before it will fit a line",
            file=sys.stderr,
        )


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; the verb surface stays frozen once published."""
    ap = argparse.ArgumentParser(prog="claude_worker.bin15_seed")
    sub = ap.add_subparsers(dest="lane", required=True)

    one = sub.add_parser("seed-out", help="cut one bin15-seed-<COIN>[-1d].tsv")
    one.add_argument("--db", default=None, type=pathlib.Path)
    one.add_argument("--family", required=True)
    one.add_argument("--descriptor", default=None)
    one.add_argument("--out", required=True, type=pathlib.Path)
    one.add_argument("--pairs", type=int, default=PAIRS_DEFAULT)
    one.add_argument("--window-minutes", type=int, default=WINDOW_MINUTES_DEFAULT)
    one.add_argument("--now-ms", type=int, default=None)
    one.add_argument("--window", dest="window", action="store_true", default=None)
    one.add_argument("--no-window", dest="window", action="store_false", default=None)

    every = sub.add_parser("seed-all", help="every file bin15.toml implies")
    every.add_argument("--db", default=None, type=pathlib.Path)
    every.add_argument("--bin15", required=True, type=pathlib.Path)
    every.add_argument("--dir", required=True, type=pathlib.Path)
    every.add_argument("--pairs", type=int, default=PAIRS_DEFAULT)
    every.add_argument("--now-ms", type=int, default=None)

    args = ap.parse_args(argv)
    db = args.db or pathlib.Path(os.path.expanduser(DEFAULT_DB))
    if args.lane == "seed-all":
        written = seed_all(db, args.bin15, args.dir, args.pairs, args.now_ms)
        if not written:
            print("bin15-seed: bin15.toml implied no seed file", file=sys.stderr)
            return 3
        for name, n, stats in written:
            print(_report(name, n, stats))
            _warn(n, stats, stats.window_minutes > 0)
        return 0

    try:
        fam = parse_family(args.family)
    except ValueError as e:
        print(f"bin15-seed: {e}", file=sys.stderr)
        return 2
    descriptor = args.descriptor or fam.default_descriptor
    spec = CutSpec(fam, descriptor, args.pairs, args.window_minutes)
    n, stats = seed_out(db, spec, args.out, args.now_ms, args.window)
    print(_report(str(args.out), n, stats))
    _warn(n, stats, stats.window_minutes > 0)
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
