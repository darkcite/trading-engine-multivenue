# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15 accrual — turn each closed UTC day's capture into ledger rows
and entry rows, so the desk gate accrues instead of ageing into S3.

The BIN15 lane's evidence is a SAMPLE SIZE problem, not an analysis
problem: 12.5 h of tape gave 32 settled entries and a 95 % interval on
the hit rate spanning 51 %-82 %. Detecting even a 10-point edge needs
~86 settled entries and a 5-point edge ~344. Run dirs are archived to
object storage and removed locally after a day (``PROTECT_DAYS=1``), so
a lane that only analyses on request analyses the same fortnight of tape
over and over. This module makes the day's evidence PERMANENT and small:
two append-only TSVs under the worker's own state root.

**What it does.** For every run of the closed day that carries a HIP-4
instance: cut it into ≤ 2 h windows (the capture-window law, absolute),
cut a POINT-IN-TIME seed for each from ``candles.db``, drive
``backtest --member bin15 --emit-detail`` through it, and merge the
sidecar's ``bin15_ledger`` and ``bin15_entries`` blocks into the stores.
Idempotent: both merges dedupe, so re-running a day adds nothing.

**Why the seed is the whole method.** ``bin15_boot``'s seed loader does
NOT filter by time (xsd's does — it drops rows at or after the boot
hour). Pointing a replay of an old window at the LIVE seed hands the
forecast hours of the future, and every ``d`` inherits it. Filtering the
live seed FILE instead starves the HAR — it holds ~25 h of minute
returns and the forecast needs 1 440 before it speaks at all. Only
re-cutting from ``candles.db`` at the window's own first instant is both
honest and warm, and that is what :func:`seed_for` does, through the
same ``bin15_seed seed-all --now-ms`` call the engine wrapper makes at
boot.

Lanes (``python -m claude_worker.bin15_accrue <lane>``):

- ``accrue [--day YYYY-MM-DD] [--replay-dir …]`` — drive the day and
  merge. ``--day`` absent = the closed UTC day, which is what the 0020
  slot wants.
- ``report`` — the three operator questions (prediction success at the
  price paid, entry timing, entry price) with a Wilson interval and the
  sample size each conclusion would need.
- ``status`` — rows, instances, settled entries, days covered.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import collections
import datetime
import json
import math
import os
import pathlib
import shutil
import statistics
import subprocess
import sys
import tempfile
import typing

import claude_worker.backtest
import claude_worker.bin15_ledger
import claude_worker.bin15_ref
import claude_worker.hip4
import claude_worker.window_root

#: The entry store, beside the calibration ledger.
DEFAULT_ENTRIES: str = "~/multivenue/worker/bin15/entries.tsv"

#: Header; `#` lines are skipped on read.
ENTRIES_HEADER: str = (
    "# bin15 entries — one row per COVERAGE ENTRY the member placed,\n"
    "#   merged from `backtest --member bin15 --emit-detail` sidecars.\n"
    "# ts_ns\toutcome\tfamily\tstart_ns\texpiry_ns\toffset_s\tis_yes\tpx_1e6\tqty_1e6\tp_hat_1e6\ty\n"
    "# offset_s: seconds from the INSTANCE's start (expiry - 900 s), not\n"
    "#   from our receipt of the roll, which lags the venue a second or two.\n"
    "# y: 1000000 settled ITM, 0 settled OTM, -1 the window could not derive\n"
    "#   the payout (its expiry or TWAP falls outside the cut).\n"
    "# Worker state. Never git; findings go to docs/research/outcome/.\n"
)

#: `y` for an entry whose instance the window could not settle.
Y_UNKNOWN: int = claude_worker.bin15_ledger.Y_UNKNOWN

#: The 15 m tenor, ns.
TAU_15M_NS: int = 900_000_000_000


class Entry(typing.NamedTuple):
    """One coverage entry, as placed and (later) as settled."""

    ts_ns: int
    outcome: int
    family: int
    start_ns: int
    expiry_ns: int
    offset_s: int
    is_yes: int
    px_1e6: int
    qty_1e6: int
    p_hat_1e6: int
    y: int

    @property
    def settled(self) -> bool:
        return self.y != Y_UNKNOWN

    @property
    def won(self) -> bool:
        """Whether the side the member BOUGHT is the side that settled."""
        return (self.y >= 500_000) == (self.is_yes == 1)

    def tsv(self) -> str:
        return "\t".join(str(v) for v in self) + "\n"


def entries_path(path: str | None = None) -> pathlib.Path:
    return pathlib.Path(os.path.expanduser(path or DEFAULT_ENTRIES))


def entries_from_sidecar(text: str) -> list[Entry]:
    """The `bin15_entries` block of one `--emit-detail` sidecar."""
    block = json.loads(text).get("bin15_entries")
    if not block:
        return []
    out: list[Entry] = []
    for r in block:
        y = r.get("y")
        out.append(
            Entry(
                ts_ns=int(r["ts_ns"]),
                outcome=int(r["outcome"]),
                family=int(r["family"]),
                start_ns=int(r["start_ns"]),
                expiry_ns=int(r["expiry_ns"]),
                offset_s=int(r["offset_s"]),
                is_yes=int(r["is_yes"]),
                px_1e6=int(r["px_1e6"]),
                qty_1e6=int(r["qty_1e6"]),
                p_hat_1e6=int(r["p_hat_1e6"]),
                y=Y_UNKNOWN if y is None else int(y),
            )
        )
    return out


def read_entries(path: pathlib.Path) -> list[Entry]:
    """Every entry, oldest first. Empty when absent."""
    if not path.is_file():
        return []
    out: list[Entry] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        f = s.split("\t")
        if len(f) != 11:
            raise ValueError(f"{path}: want 11 columns, got {len(f)}: {s!r}")
        out.append(Entry(*(int(v) for v in f)))
    return out


def merge_entries(
    existing: typing.Sequence[Entry], incoming: typing.Sequence[Entry]
) -> tuple[list[Entry], int]:
    """``(merged oldest first, added)``, deduped by OUTCOME.

    One instance is entered at most once, so the outcome id is the
    identity. A SETTLED row wins over an unsettled one whichever way
    round they arrive: a window re-cut later can carry the same entry
    with its payout now derivable, and that is an upgrade, not a
    duplicate. Otherwise existing wins, so a re-cut can never un-settle
    the store.
    """
    by: dict[int, Entry] = {e.outcome: e for e in existing}
    added = 0
    for e in incoming:
        old = by.get(e.outcome)
        if old is None:
            by[e.outcome] = e
            added += 1
        elif not old.settled and e.settled:
            by[e.outcome] = e
    return sorted(by.values(), key=lambda e: (e.ts_ns, e.outcome)), added


def write_entries(path: pathlib.Path, rows: typing.Sequence[Entry]) -> None:
    """Atomic write (tmp + rename): a merge never leaves a torn file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(ENTRIES_HEADER)
        for r in rows:
            f.write(r.tsv())
    os.replace(tmp, path)


# ---------------------------------------------------------------------
# driving the harness
# ---------------------------------------------------------------------


def day_of(epoch_ns: int) -> str:
    return datetime.datetime.fromtimestamp(
        epoch_ns / 1e9, datetime.UTC
    ).strftime("%Y-%m-%d")


def hip4_runs(replay_dir: pathlib.Path, day: str | None) -> list[pathlib.Path]:
    """Run dirs of ``day`` (all days when None) that carry a HIP-4 roll.

    A run with no rolling family holds no instance and is skipped rather
    than audited for nothing.
    """
    out: list[pathlib.Path] = []
    for p in sorted(replay_dir.glob("run-*")):
        try:
            epoch = int(p.name.split("-")[1])
        except (IndexError, ValueError):
            continue
        if day is not None and day_of(epoch) != day:
            continue
        try:
            if claude_worker.hip4.read_rolls(p):
                out.append(p)
        except Exception:  # a torn or unreadable events file is not a run
            continue
    return out


def cutoff_ms_of(run: pathlib.Path, lo_s: float) -> int:
    """A window's first wall instant, ms — the seed cutoff.

    From the run dir's NAME (`run-<epoch_ns>` is its start wall time)
    plus the cut's offset, not from the PMLR stamps: those are MONOTONIC
    and turning them into wall needs the anchor, which is one more thing
    to get wrong for a number the filename already states exactly.
    """
    return (int(run.name.split("-")[1]) + int(lo_s * 1_000_000_000)) // 1_000_000


def seed_for(bin15: pathlib.Path, dst: pathlib.Path, cutoff_ms: int) -> bool:
    """Cut a seed AS OF ``cutoff_ms`` from candles.db — the same
    ``bin15_seed seed-all --now-ms`` the engine wrapper runs at boot.
    See the module docstring for why nothing cheaper is honest."""
    dst.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(
        [
            sys.executable, "-m", "claude_worker.bin15_seed", "seed-all",
            "--bin15", str(bin15), "--dir", str(dst), "--now-ms", str(cutoff_ms),
        ],
        capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        print(
            f"bin15-accrue: seed-all @{cutoff_ms} exit {proc.returncode}: "
            f"{proc.stderr.strip()[-200:]}",
            file=sys.stderr,
        )
        return False
    return True


def drive(
    unit: pathlib.Path,
    bin15: pathlib.Path,
    seed_dir: pathlib.Path,
    detail: pathlib.Path,
    fee_flag: str,
    run_fn: typing.Callable[[list[str]], int] | None = None,
) -> bool:
    """One ≤ 2 h window through `backtest --member bin15`."""
    argv = [
        claude_worker.backtest.ENGINE_BINARY, "backtest",
        "--replay-dir", str(unit),
        # All-OOS: there is no ruleset being fitted here, so an IS half
        # would only hide instances from the count.
        "--split", "0/100",
        "--member", "bin15",
        "--bin15", str(bin15),
        "--bin15-seed-dir", str(seed_dir),
        "--fee-bps", fee_flag,
        "--emit-detail", str(detail),
    ]
    if run_fn is not None:
        return run_fn(argv) == 0
    proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        print(
            f"bin15-accrue: {unit.name}: harness exit {proc.returncode}: "
            f"{proc.stderr.strip()[-300:]}",
            file=sys.stderr,
        )
        return False
    return True


def accrue(
    replay_dir: pathlib.Path,
    bin15: pathlib.Path,
    day: str | None,
    ledger: pathlib.Path,
    entries: pathlib.Path,
    fee_flag: str,
    report: typing.Callable[[str], None] = lambda s: None,
) -> tuple[int, int, int]:
    """``(windows driven, ledger rows added, entries added)``."""
    runs = hip4_runs(replay_dir, day)
    if not runs:
        report(f"bin15-accrue: no HIP-4 run for {day or 'any day'}")
        return (0, 0, 0)
    new_ledger: list = []
    new_entries: list[Entry] = []
    units = 0
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        for run in runs:
            try:
                cuts = claude_worker.window_root.windows_of(run)
            except claude_worker.window_root.WindowError as exc:
                report(f"bin15-accrue: {run.name}: cannot window ({exc}) — skipped")
                continue
            for lo, hi in cuts:
                unit = root / f"unit-{run.name}-{lo:.0f}"
                shutil.rmtree(unit, ignore_errors=True)
                unit.mkdir(parents=True)
                try:
                    claude_worker.window_root.cut_run(run, unit, lo, hi)
                except claude_worker.window_root.WindowError as exc:
                    report(f"bin15-accrue: {run.name}@{lo:.0f}s: cut failed ({exc})")
                    shutil.rmtree(unit, ignore_errors=True)
                    continue
                seed_dir = root / f"seed-{unit.name}"
                detail = root / f"{unit.name}.json"
                units += 1
                if seed_for(bin15, seed_dir, cutoff_ms_of(run, lo)) and drive(
                    unit, bin15, seed_dir, detail, fee_flag
                ) and detail.is_file():
                    text = detail.read_text(encoding="utf-8")
                    new_ledger.extend(
                        claude_worker.bin15_ledger.rows_from_sidecar(text)
                    )
                    new_entries.extend(entries_from_sidecar(text))
                shutil.rmtree(unit, ignore_errors=True)
                shutil.rmtree(seed_dir, ignore_errors=True)
    merged_l, added_l = claude_worker.bin15_ledger.merge(
        claude_worker.bin15_ledger.read_ledger(ledger), new_ledger
    )
    claude_worker.bin15_ledger.write_ledger(ledger, merged_l)
    merged_e, added_e = merge_entries(read_entries(entries), new_entries)
    write_entries(entries, merged_e)
    return (units, added_l, added_e)


# ---------------------------------------------------------------------
# the report
# ---------------------------------------------------------------------


def wilson(k: int, n: int, z: float = 1.96) -> tuple[float, float]:
    """Wilson score interval on ``k/n`` — the one that stays inside
    [0, 1] and does not lie at the small n this lane has."""
    if n == 0:
        return (0.0, 1.0)
    den = n + z * z
    centre = (k + z * z / 2) / den
    half = (z / den) * math.sqrt(k * (n - k) / n + z * z / 4)
    return (max(0.0, centre - half), min(1.0, centre + half))


def needed_n(p: float, edge: float, z: float = 2.0) -> int:
    """Settled entries to resolve ``edge`` at ``z`` SE, at hit rate p."""
    if edge <= 0:
        return 0
    return int(math.ceil(z * z * p * (1 - p) / (edge * edge)))


def render(entries: typing.Sequence[Entry], fee_bps: int) -> list[str]:
    """The three operator questions, with the error bar that decides
    whether any of them is an answer yet."""
    out: list[str] = []
    settled = [e for e in entries if e.settled]
    days = {day_of(e.ts_ns) for e in entries}
    out.append(
        f"entries {len(entries)} over {len(days)} UTC day(s); "
        f"{len(settled)} settled, {len(entries)-len(settled)} straddled a window "
        "edge or a restart gap (counted, never guessed)"
    )
    if not settled:
        return out + ["no settled entry yet: nothing to score"]

    n = len(settled)
    k = sum(1 for e in settled if e.won)
    p = k / n
    avg_px = statistics.fmean(e.px_1e6 / 1e6 for e in settled)
    cost = sum(e.px_1e6 / 1e6 * e.qty_1e6 / 1e6 for e in settled)
    payout = sum(e.qty_1e6 / 1e6 for e in settled if e.won)
    fee = payout * fee_bps / 10_000.0
    lo, hi = wilson(k, n)
    se = math.sqrt(p * (1 - p) / n)

    out.append("")
    out.append(f"1. PREDICTION SUCCESS: {k}/{n} = {100*p:.1f} %")
    out.append(f"   average price paid {avg_px:.4f} => break-even hit rate {100*avg_px:.1f} %")
    # DOLLARS are the verdict, not the hit rate and not a ratio of
    # averages: a win at 0.96 returns 4 c on 96 c staked while a win at
    # 0.48 returns 52 c on 48 c, so `p/a - 1` at the MEAN price can
    # disagree in sign with what the account did. It has, on this lane.
    # `+$150.40`, not `$+150.40`: the sign belongs to the amount.
    def usd(v: float) -> str:
        return f"{'-' if v < 0 else '+'}${abs(v):,.2f}"

    out.append(
        f"   AS TRADED: cost ${cost:,.2f}  payout ${payout:,.2f}  "
        f"gross {usd(payout-cost)} ({100*(payout-cost)/cost:+.2f} %)  "
        f"net of the {fee_bps} bps exit leg {usd(payout-cost-fee)}"
    )
    out.append(
        f"   Wilson 95 % CI on the hit rate [{100*lo:.1f} %, {100*hi:.1f} %] — "
        f"break-even {100*avg_px:.1f} % is "
        f"{'INSIDE (nothing is settled yet)' if lo <= avg_px <= hi else 'OUTSIDE'}"
    )
    if se > 0:
        out.append(
            f"   edge over break-even {100*(p-avg_px):+.1f} pts = {(p-avg_px)/se:.2f} SD; "
            "settled entries needed to resolve, at 2 SE:"
        )
        for e_ in (0.05, 0.08, 0.10):
            need = needed_n(p, e_)
            more = max(0, need - n)
            out.append(
                f"     {100*e_:>3.0f} pts -> n = {need:5d}  ({more} more, "
                f"~{more/96:.1f} days at 96 instances/day on one family)"
            )
    for ph in range(claude_worker.bin15_ref.PHASES):
        sub = [
            e for e in settled
            if claude_worker.bin15_ref.phase_of(max(e.expiry_ns - e.ts_ns, 1)) == ph
        ]
        if sub:
            sk = sum(1 for e in sub if e.won)
            spx = statistics.fmean(e.px_1e6 / 1e6 for e in sub)
            flag = "  <-- under its price" if sk / len(sub) < spx else ""
            out.append(
                f"   phase {claude_worker.bin15_ledger.PHASE_NAMES[ph]:<5}: "
                f"{sk}/{len(sub)} = {100*sk/len(sub):5.1f} %  "
                f"break-even {100*spx:5.1f} %{flag}"
            )

    offs = sorted(e.offset_s for e in entries)
    out.append("")
    out.append("2. ENTRY TIMING, seconds from the instance's start (0 .. 900):")
    out.append(
        f"   min {offs[0]}  p25 {offs[len(offs)//4]}  median {offs[len(offs)//2]}  "
        f"p75 {offs[3*len(offs)//4]}  max {offs[-1]}  mean {statistics.fmean(offs):.0f}"
    )
    buckets = collections.Counter(min(o // 60, 14) for o in offs)
    for b in range(15):
        if buckets[b]:
            out.append(f"   [{b*60:3d}-{b*60+59:3d}s] {buckets[b]:4d} {'#' * min(buckets[b], 50)}")

    px = [e.px_1e6 / 1e6 for e in entries]
    qty = [e.qty_1e6 / 1e6 for e in entries]
    out.append("")
    out.append("3. ENTRY PRICE (all entries, settled or not):")
    out.append(
        f"   mean {statistics.fmean(px):.4f}  median {statistics.median(px):.4f}  "
        f"min {min(px):.4f}  max {max(px):.4f}"
    )
    out.append(
        f"   size mean {statistics.fmean(qty):.0f} contracts, mean notional "
        f"${statistics.fmean(p_ * q for p_, q in zip(px, qty)):.2f}"
    )
    yes = sum(1 for e in entries if e.is_yes == 1)
    out.append(f"   side {yes} YES / {len(entries)-yes} NO")
    return out


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; the verb surface stays frozen once published."""
    ap = argparse.ArgumentParser(prog="claude_worker.bin15_accrue")
    sub = ap.add_subparsers(dest="lane", required=True)

    ac = sub.add_parser("accrue", help="drive the day and merge into the stores")
    ac.add_argument("--day", default=None, help="YYYY-MM-DD; absent = the closed UTC day")
    ac.add_argument("--replay-dir", default=None)
    ac.add_argument("--bin15", default=None)
    ac.add_argument("--ledger", default=None)
    ac.add_argument("--entries", default=None)
    ac.add_argument("--fee-bps", default="hl.prediction:0:0")

    rp = sub.add_parser("report", help="the three questions, with the error bar")
    rp.add_argument("--entries", default=None)
    rp.add_argument("--fee-bps-exit", type=int, default=0)

    st = sub.add_parser("status", help="rows, instances, settled, days")
    st.add_argument("--entries", default=None)

    args = ap.parse_args(argv)

    if args.lane == "accrue":
        replay = pathlib.Path(
            os.path.expanduser(args.replay_dir or "~/multivenue/logs")
        )
        bin15 = pathlib.Path(os.path.expanduser(args.bin15 or "~/multivenue/bin15.toml"))
        if not bin15.is_file():
            print(f"bin15-accrue: {bin15}: no such artifact", file=sys.stderr)
            return 2
        day = args.day
        if day is None:
            now = datetime.datetime.now(datetime.UTC)
            day = (now - datetime.timedelta(days=1)).strftime("%Y-%m-%d")
        led = claude_worker.bin15_ledger.ledger_path(args.ledger)
        ent = entries_path(args.entries)
        units, dl, de = accrue(
            replay, bin15, day, led, ent, args.fee_bps, lambda s: print(s, file=sys.stderr)
        )
        print(
            f"bin15-accrue: day {day}: {units} window(s) <= 2 h driven; "
            f"+{dl} ledger row(s) -> {led}; +{de} entry(ies) -> {ent}"
        )
        return 0

    ent = entries_path(args.entries)
    rows = read_entries(ent)
    if args.lane == "status":
        settled = [e for e in rows if e.settled]
        print(
            f"bin15-accrue: {ent} entries={len(rows)} settled={len(settled)} "
            f"days={len({day_of(e.ts_ns) for e in rows})} "
            f"families={len({e.family for e in rows})}"
        )
        return 0

    print(f"bin15-accrue: {ent}")
    for line in render(rows, args.fee_bps_exit):
        print(f"  {line}")
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
