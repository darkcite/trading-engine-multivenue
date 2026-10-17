#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""PVM B'3 cut feeder (impl-Bp §3.7, §4.7): pull -> cut -> READY; DONE -> evict.

Subcommands:

    plan     --window-ms A:B --hold-ms H --seed S --out plan.tsv [--listing FILE]
    feed     --plan plan.tsv --spool DIR --cutter TEMPLATE [--listing FILE]
    selftest

``plan`` applies the spread rule BEFORE a byte moves: complete 2 h windows of
v3 runs inside [A, B), one cut per stratum of n = max(8, ceil(20 H / 2 h)),
then span >= 30 H, tape >= 20 H and no sorted-phase gap wider than H / 4 (the
constants B'3's ``check_plan`` re-verifies). Unsatisfiable -> exit 3 with the
shortfall ("Unscorable": never a zero).

``feed`` materialises each ``spool:<k>`` cut as ``<spool>/<k>/`` and marks it
``<k>.ready`` (write tmp -> fsync -> rename), prefetching one cut ahead
(pin:Bp-feed-prefetch-one: cut k is pulled once cut k - 2 is DONE, so at most
the cut being replayed and the next one exist), evicts ``<k>/`` once
``<k>.done`` appears, refuses (``<k>.refused`` + exit 5) below the pull floor
(25 GiB + the run's size), never waits for space, and stops and evicts all on
B'3's ``abort``. Local rows (an absolute root) are read in place: nothing is
copied or evicted for them.

pin:Bp-cutter-template — the worker's window cutter (its name and layout are
T0.5, read on the Mac) is passed as ``--cutter`` with the placeholders
``{run_id} {run_epoch_ns} {off_from_ns} {off_to_ns} {out}``; the pull is
``multivenue-archive pull <run_id>``.

pin:Bp-local-row-whole-run — ``plan`` emits a local row only for a run whose
whole capture fits one window at offset 0 (B'3 refuses a cut longer than it
declares, and a local row is read straight from its run directory).

The CMDLINE LAW: run as ``~/multivenue/venv/bin/python3 <repo>/scripts/pvm-cut-feed.py``
(no path here matches ``claude[-_]worke[r]``); ``plan`` and ``feed`` refuse to
start while the worker guard (``pgrep -f 'claude[-_]worke[r]'``,
``pgrep -f pytes[t]``) is non-empty. Stdlib only; no worker database is
touched.
"""

import argparse
import json
import math
import os
import random
import shutil
import subprocess
import sys
import tempfile
import threading
import time

SCHEMA = "pvm-cutplan-1"
WINDOW_NS = 7_200_000_000_000
SPAN_HOLDS = 30
TAPE_HOLDS = 20
PHASE_DIV = 4
MIN_STRATA = 8
FLOOR_BYTES = 25 * 1024**3
EXIT_UNSCORABLE = 3
EXIT_REFUSED = 5
NS_PER_MS = 1_000_000


class Unscorable(Exception):
    """The archive cannot score this hold horizon (exit 3)."""


# ---------------------------------------------------------------- plan


def candidates(listing, a_ns, b_ns):
    """Complete 2 h windows of v3 runs inside [a_ns, b_ns)."""
    out = []
    for run in listing:
        if int(run.get("version", 0)) < 3:
            continue  # a v2 root is stale-blind (CLAUDE.md pitfall 12)
        epoch = int(run["epoch_ns"])
        local = bool(run.get("local", False))
        if local:
            span = int(run["span_ns"])
            if span < WINDOW_NS and a_ns <= epoch and epoch + span + 1 <= b_ns:
                out.append(
                    {
                        "run": run,
                        "from": 0,
                        "to": span + 1,
                        "start": epoch,
                        "local": True,
                    }
                )
            continue
        k = 0
        while k < int(run.get("windows_2h_complete", 0)):
            start = epoch + k * WINDOW_NS
            if a_ns <= start and start + WINDOW_NS <= b_ns:
                out.append(
                    {
                        "run": run,
                        "from": k * WINDOW_NS,
                        "to": (k + 1) * WINDOW_NS,
                        "start": start,
                        "local": False,
                    }
                )
            k += 1
    return out


def spread_check(cuts, hold_ns):
    """None when the spread rule holds, else the shortfall."""
    if not cuts:
        return "no cut"
    first = cuts[0]["start"]
    last = cuts[-1]["start"] + (cuts[-1]["to"] - cuts[-1]["from"])
    if last - first < SPAN_HOLDS * hold_ns:
        return "span %d ns < %d holds" % (last - first, SPAN_HOLDS)
    tape = sum(c["to"] - c["from"] for c in cuts)
    if tape < TAPE_HOLDS * hold_ns:
        return "tape %d ns < %d holds" % (tape, TAPE_HOLDS)
    phases = sorted(c["start"] % hold_ns for c in cuts)
    i = 1
    while i < len(phases):
        if phases[i] - phases[i - 1] > hold_ns // PHASE_DIV:
            return "phase gap %d ns > hold / %d" % (phases[i] - phases[i - 1], PHASE_DIV)
        i += 1
    return None


def make_plan(listing, a_ms, b_ms, hold_ms, seed):
    """The plan rows (dicts), or raise Unscorable."""
    a_ns, b_ns, hold_ns = a_ms * NS_PER_MS, b_ms * NS_PER_MS, hold_ms * NS_PER_MS
    if b_ns <= a_ns or hold_ns <= 0:
        raise Unscorable("empty window or non-positive hold")
    n = max(MIN_STRATA, math.ceil(TAPE_HOLDS * hold_ns / WINDOW_NS))
    cands = candidates(listing, a_ns, b_ns)
    rng = random.Random(seed)
    tie = {id(c): rng.random() for c in cands}
    width = (b_ns - a_ns) / n
    picked = []
    s = 0
    while s < n:
        lo = a_ns + s * width
        hi = a_ns + (s + 1) * width
        here = [c for c in cands if lo <= c["start"] < hi]
        if here:
            here.sort(
                key=lambda c: (
                    not c["local"],
                    -int(c["run"].get("bytes", 0)),
                    c["start"],
                    tie[id(c)],
                )
            )
            picked.append(here[0])
        s += 1
    picked.sort(key=lambda c: c["start"])
    # Disjointness (a window never straddles a stratum twice, but a local
    # whole-run row may overlap a neighbour's window): drop overlaps.
    rows = []
    for c in picked:
        if rows:
            prev = rows[-1]
            if c["start"] < prev["start"] + (prev["to"] - prev["from"]):
                continue
        rows.append(c)
    if len(rows) < n:
        raise Unscorable("%d of %d strata have a complete window" % (len(rows), n))
    why = spread_check(rows, hold_ns)
    if why is not None:
        raise Unscorable(why)
    return rows


def render_plan(rows):
    lines = ["# " + SCHEMA, "# cut\trun_id\trun_epoch_ns\toff_from_ns\toff_to_ns\troot"]
    for k, c in enumerate(rows):
        run = c["run"]
        epoch = int(run["epoch_ns"])
        root = str(run["path"]) if c["local"] else "spool:%d" % k
        lines.append("%d\trun-%d\t%d\t%d\t%d\t%s" % (k, epoch, epoch, c["from"], c["to"], root))
    return "\n".join(lines) + "\n"


def parse_plan(text):
    rows = []
    seen = False
    for line in text.splitlines():
        if not line.strip():
            continue
        if not seen:
            if line.strip() != "# " + SCHEMA:
                raise ValueError("the first line must be '# %s'" % SCHEMA)
            seen = True
            continue
        if line.startswith("#"):
            continue
        f = line.split("\t")
        if len(f) != 6:
            raise ValueError("want 6 columns: %r" % line)
        rows.append(
            {
                "cut": int(f[0]),
                "run_id": f[1],
                "run_epoch_ns": int(f[2]),
                "off_from_ns": int(f[3]),
                "off_to_ns": int(f[4]),
                "root": f[5],
            }
        )
    return rows


# ---------------------------------------------------------------- feed


def write_marker(spool, name, body=""):
    """write tmp -> fsync -> rename."""
    tmp = os.path.join(spool, "." + name + ".tmp")
    with open(tmp, "w") as fh:
        fh.write(body)
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(tmp, os.path.join(spool, name))


def wait_for(path, abort, poll=0.05, max_wait=7_200.0):
    """True once `path` exists; False on abort or the 2 h bound."""
    start = time.monotonic()
    while not os.path.exists(path):
        if os.path.exists(abort) or time.monotonic() - start > max_wait:
            return False
        time.sleep(poll)
    return True


def evict(spool, k):
    shutil.rmtree(os.path.join(spool, str(k)), ignore_errors=True)


def feed(rows, spool, materialise, free_bytes, run_bytes, poll=0.05, max_wait=7_200.0):
    """Feed every spool row; returns the exit code."""
    abort = os.path.join(spool, "abort")
    held = []
    for row in rows:
        k = row["cut"]
        if k >= 2 and not wait_for(os.path.join(spool, "%d.done" % (k - 2)), abort, poll, max_wait):
            break
        if os.path.exists(abort):
            break
        for h in list(held):
            if os.path.exists(os.path.join(spool, "%d.done" % h)):
                evict(spool, h)
                held.remove(h)
        if not row["root"].startswith("spool:"):
            continue  # a local row is read in place
        need = FLOOR_BYTES + run_bytes(row["run_id"])
        have = free_bytes(spool)
        if have < need:
            write_marker(spool, "%d.refused" % k, "pull floor: %d GiB free\n" % (have // 1024**3))
            return EXIT_REFUSED
        out = os.path.join(spool, str(k))
        materialise(row, out)
        held.append(k)
        write_marker(spool, "%d.ready" % k)
    if os.path.exists(abort):
        for h in held:
            evict(spool, h)
        return 0
    for h in held:
        wait_for(os.path.join(spool, "%d.done" % h), abort, poll, max_wait)
        evict(spool, h)
    return 0


def archive_materialiser(template):
    def run(row, out):
        os.makedirs(out, exist_ok=True)
        subprocess.run(["multivenue-archive", "pull", row["run_id"]], check=True)
        cmd = template.format(
            run_id=row["run_id"],
            run_epoch_ns=row["run_epoch_ns"],
            off_from_ns=row["off_from_ns"],
            off_to_ns=row["off_to_ns"],
            out=out,
        )
        subprocess.run(cmd, shell=True, check=True)

    return run


def load_listing(path):
    if path:
        with open(path) as fh:
            return json.load(fh)
    out = subprocess.run(["multivenue-archive", "list", "--json"], check=True, capture_output=True, text=True)
    return json.loads(out.stdout)


def worker_busy():
    for pat in ("claude[-_]worke[r]", "pytes[t]"):
        r = subprocess.run(["pgrep", "-f", pat], capture_output=True)
        if r.returncode == 0:
            return True
    return False


# ---------------------------------------------------------------- selftest


def _listing_spread(hold_ms):
    """Runs every 61 min for 3 days, each with one complete 2 h window."""
    runs = []
    t0 = 1_758_240_000_000_000_000
    i = 0
    while i < 72:
        runs.append(
            {
                "run_id": "run-%d" % (t0 + i * 3_660 * 10**9),
                "epoch_ns": t0 + i * 3_660 * 10**9 * 3,
                "span_ns": WINDOW_NS + 1,
                "windows_2h_complete": 1,
                "version": 3,
                "bytes": 10**9,
            }
        )
        i += 1
    a_ms = t0 // NS_PER_MS
    b_ms = (t0 + 72 * 3 * 3_660 * 10**9) // NS_PER_MS
    return runs, a_ms, b_ms, hold_ms


def _case_newest_k():
    runs, a_ms, b_ms, _ = _listing_spread(60_000)
    newest = runs[-4:]  # a newest-K listing: four recent runs only
    try:
        make_plan(newest, a_ms, b_ms, 3_600_000, 7)
    except Unscorable:
        return True
    return False


def _case_spread():
    runs, a_ms, b_ms, hold = _listing_spread(60_000)
    rows = make_plan(runs, a_ms, b_ms, hold, 7)
    text = render_plan(rows)
    back = parse_plan(text)
    if len(back) < MIN_STRATA:
        return False
    i = 1
    while i < len(back):
        p, c = back[i - 1], back[i]
        if p["run_epoch_ns"] + p["off_to_ns"] > c["run_epoch_ns"] + c["off_from_ns"]:
            return False
        i += 1
    return make_plan(runs, a_ms, b_ms, hold, 7) == rows  # deterministic


def _spool_rows(n):
    return [
        {"cut": k, "run_id": "run-1", "run_epoch_ns": 1, "off_from_ns": 0, "off_to_ns": 5, "root": "spool:%d" % k}
        for k in range(n)
    ]


def _fake_materialise(row, out):
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "bn-ticks.pmlr"), "w") as fh:
        fh.write("cut %d" % row["cut"])


def _consumer(spool, n, stop_at=None):
    """A stand-in for B'3: READY -> read -> DONE (or abort at `stop_at`)."""
    seen = []
    for k in range(n):
        if k == stop_at:
            write_marker(spool, "abort")
            return seen
        ready = os.path.join(spool, "%d.ready" % k)
        refused = os.path.join(spool, "%d.refused" % k)
        while not os.path.exists(ready):
            if os.path.exists(refused):
                write_marker(spool, "abort")
                return seen
            time.sleep(0.01)
        with open(os.path.join(spool, str(k), "bn-ticks.pmlr")) as fh:
            seen.append(fh.read())
        write_marker(spool, "%d.done" % k)
    return seen


def _run_feed(n, free, stop_at=None):
    spool = tempfile.mkdtemp(prefix="pvm-feed-")
    result = {}

    def go():
        result["code"] = feed(_spool_rows(n), spool, _fake_materialise, lambda _: free, lambda _: 0, poll=0.01, max_wait=5.0)

    t = threading.Thread(target=go)
    t.start()
    seen = _consumer(spool, n, stop_at)
    t.join(10)
    left = sorted(e for e in os.listdir(spool) if e.isdigit())
    shutil.rmtree(spool, ignore_errors=True)
    return result.get("code"), seen, left


def _case_handshake():
    code, seen, left = _run_feed(4, 10**15)
    return code == 0 and seen == ["cut %d" % k for k in range(4)] and left == []


def _case_abort():
    code, seen, left = _run_feed(4, 10**15, stop_at=2)
    return code == 0 and seen == ["cut 0", "cut 1"] and left == []


def _case_refused():
    code, seen, _ = _run_feed(3, 0)
    return code == EXIT_REFUSED and seen == []


def selftest():
    cases = [
        ("newest_k_listing_is_unscorable", _case_newest_k),
        ("spread_listing_plans_disjoint_cuts", _case_spread),
        ("ready_done_evict", _case_handshake),
        ("abort_stops_and_evicts", _case_abort),
        ("pull_floor_refuses", _case_refused),
    ]
    for name, fn in cases:
        if not fn():
            print(name)
            return 1
    print("pvm-cut-feed selftest: %d cases ok" % len(cases))
    return 0


# ---------------------------------------------------------------- main


def main(argv=None):
    ap = argparse.ArgumentParser(prog="pvm-cut-feed")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("plan")
    p.add_argument("--window-ms", required=True)
    p.add_argument("--hold-ms", required=True, type=int)
    p.add_argument("--seed", required=True, type=int)
    p.add_argument("--out", required=True)
    p.add_argument("--listing")
    f = sub.add_parser("feed")
    f.add_argument("--plan", required=True)
    f.add_argument("--spool", required=True)
    f.add_argument("--cutter", required=True)
    f.add_argument("--listing")
    sub.add_parser("selftest")
    args = ap.parse_args(argv)
    if args.cmd == "selftest":
        return selftest()
    if worker_busy():
        print("pvm-cut-feed: the worker guard is busy; refusing to start", file=sys.stderr)
        return 2
    if args.cmd == "plan":
        a, b = (int(x) for x in args.window_ms.split(":"))
        try:
            rows = make_plan(load_listing(args.listing), a, b, args.hold_ms, args.seed)
        except Unscorable as e:
            print("Unscorable: %s" % e, file=sys.stderr)
            return EXIT_UNSCORABLE
        with open(args.out, "w") as fh:
            fh.write(render_plan(rows))
        return 0
    with open(args.plan) as fh:
        rows = parse_plan(fh.read())
    sizes = {r.get("run_id"): int(r.get("bytes", 0)) for r in load_listing(args.listing)}
    os.makedirs(args.spool, exist_ok=True)
    return feed(
        rows,
        args.spool,
        archive_materialiser(args.cutter),
        lambda d: shutil.disk_usage(d).free,
        lambda run_id: sizes.get(run_id, 0),
    )


if __name__ == "__main__":
    sys.exit(main())
