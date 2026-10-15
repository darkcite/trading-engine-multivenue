#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Bench-regression gate for the `hot_path` criterion bench.

For every benchmark it runs, criterion writes `new/benchmark.json`, whose
`full_id` is the id the bench gave `bench_function` (for example
`ring/push_ref_pop_ref_tick`), and `new/estimates.json` (the median, in
ns) into one directory under `<target>/criterion/`. The directory name is
criterion's own rendering of the id (`/` becomes `_`), so this script finds
results by `full_id`, never by path: it indexes every `new/benchmark.json`,
then compares each sample in `crates/bench/baselines/hot_path.json` against
its median.

Exit status:
  0  every baselined sample has a result within `tolerance_pct`
  1  a sample regressed beyond `tolerance_pct`, or has no result, or (with
     `--since`) only a result older than this run: a gate that passes on
     nothing proves nothing
  2  the baseline file is absent or malformed

`--since EPOCH` rejects results written before EPOCH (Unix seconds), so a
result left over from an earlier run, or from a bench since deleted, cannot
stand in for this run's. `make bench-check` passes the time it started, and
the script then also lists the benches that ran without a baseline.

Usage:
    make bench-check
or by hand, after `cargo bench -p bench --bench hot_path`:
    python3 crates/bench/baselines/check_regression.py

Full `import x` only (project-wide Python rule). Python >= 3.9: the Mac's
system Python is 3.9.6.
"""

import argparse
import json
import os
import pathlib
import sys


def load_json(path: pathlib.Path) -> object:
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def index_results(criterion_dir: pathlib.Path) -> dict:
    """Map each criterion `full_id` to its newest `new/estimates.json`."""
    results = {}
    if not criterion_dir.is_dir():
        return results
    for meta in sorted(criterion_dir.glob("**/new/benchmark.json")):
        estimates = meta.parent / "estimates.json"
        if not estimates.is_file():
            continue
        full_id = load_json(meta).get("full_id")
        if not isinstance(full_id, str):
            continue
        known = results.get(full_id)
        if known is None or estimates.stat().st_mtime > known.stat().st_mtime:
            results[full_id] = estimates
    return results


def is_fresh(path: pathlib.Path, since: object) -> bool:
    return since is None or path.stat().st_mtime >= since


def is_positive_number(value: object) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and value > 0


def main() -> int:
    ap = argparse.ArgumentParser(description="Gate the hot_path bench against its baseline.")
    ap.add_argument("--baseline", default="crates/bench/baselines/hot_path.json")
    ap.add_argument(
        "--target-dir",
        default=os.environ.get("CARGO_TARGET_DIR", "target"),
    )
    ap.add_argument(
        "--since",
        type=float,
        default=None,
        metavar="EPOCH",
        help="reject results written before this Unix time (seconds)",
    )
    args = ap.parse_args()

    baseline_path = pathlib.Path(args.baseline)
    if not baseline_path.is_file():
        print(f"baseline {baseline_path} not found", file=sys.stderr)
        return 2
    base = load_json(baseline_path)
    tol_pct = base.get("tolerance_pct") if isinstance(base, dict) else None
    samples = base.get("samples") if isinstance(base, dict) else None
    if not is_positive_number(tol_pct) or not isinstance(samples, dict) or not samples:
        print(
            f"baseline {baseline_path}: needs a positive `tolerance_pct` and a non-empty `samples` map",
            file=sys.stderr,
        )
        return 2
    for bench_id, baseline_ns in samples.items():
        if not is_positive_number(baseline_ns):
            print(f"baseline {baseline_path}: `{bench_id}` needs a positive median (ns)", file=sys.stderr)
            return 2

    results = index_results(pathlib.Path(args.target_dir) / "criterion")

    width = max(len(bench_id) for bench_id in samples) + 2
    print(f"{'bench':<{width}}{'baseline ns':>14}{'observed ns':>14}{'delta':>10}")
    regressed = []
    absent = []
    faster = []
    for bench_id, baseline_ns in samples.items():
        estimates = results.get(bench_id)
        if estimates is None or not is_fresh(estimates, args.since):
            why = "missing" if estimates is None else "stale"
            absent.append(f"{bench_id}: {why}")
            print(f"{bench_id:<{width}}{baseline_ns:>14.3f}{'<' + why + '>':>14}")
            continue
        observed = float(load_json(estimates)["median"]["point_estimate"])
        delta_pct = (observed - baseline_ns) / baseline_ns * 100.0
        mark = ""
        if delta_pct > tol_pct:
            regressed.append(f"{bench_id}: {delta_pct:+.1f}%")
            mark = "  REGRESSED"
        elif delta_pct < -tol_pct:
            faster.append(f"{bench_id}: {delta_pct:+.1f}%")
            mark = "  faster"
        print(f"{bench_id:<{width}}{baseline_ns:>14.3f}{observed:>14.3f}{delta_pct:>+9.1f}%{mark}")

    if args.since is not None:
        unbaselined = sorted(
            bench_id
            for bench_id, estimates in results.items()
            if bench_id not in samples and is_fresh(estimates, args.since)
        )
        if unbaselined:
            print(f"\nran without a baseline (not gated): {', '.join(unbaselined)}")

    print()
    if faster:
        print(
            f"NOTE: {len(faster)} sample(s) beat the baseline by more than {tol_pct:g}%; "
            "re-baseline, or a later slide back to the old number passes unseen:"
        )
        for line in faster:
            print(f"  - {line}")
    if absent:
        print(f"FAIL: {len(absent)} baselined sample(s) have no result from this run:")
        for line in absent:
            print(f"  - {line}")
    if regressed:
        print(f"FAIL: {len(regressed)} regression(s) beyond {tol_pct:g}%:")
        for line in regressed:
            print(f"  - {line}")
    if absent or regressed:
        return 1
    print(f"OK: all {len(samples)} samples within {tol_pct:g}% of the baseline.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
