# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#!/usr/bin/env python3
"""Bench-regression detector.

Parses `target/criterion/<bench>/<sample>/estimates.json` files produced by
the last `cargo bench -p bench --bench hot_path` run, compares each sample's
median against `crates/bench/baselines/hot_path.json`, and prints a table.

Exits non-zero when any sample regresses beyond `tolerance_pct`.

Usage:
    cargo bench -p bench --bench hot_path
    python3 crates/bench/baselines/check_regression.py

Add `--baseline path/to/file.json` to compare against a different baseline.

Full `import x` only (project-wide Python rule).
"""

import argparse
import json
import math
import os
import pathlib
import sys


def load_baseline(p: pathlib.Path) -> dict:
    with open(p, "r", encoding="utf-8") as f:
        return json.load(f)


def find_criterion_median(target_dir: pathlib.Path, bench: str) -> float | None:
    # `cargo bench` lands `target/criterion/<group>/<bench_name>/new/estimates.json`.
    # The bench name we pass criterion is `group/name` so we split.
    group, _, name = bench.partition("/")
    if not group or not name:
        return None
    est = target_dir / "criterion" / group / name / "new" / "estimates.json"
    if not est.exists():
        return None
    with open(est, "r", encoding="utf-8") as f:
        data = json.load(f)
    # Criterion writes `median.point_estimate` in nanoseconds.
    return float(data.get("median", {}).get("point_estimate", math.nan))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--baseline",
        default="crates/bench/baselines/hot_path.json",
    )
    ap.add_argument(
        "--target-dir",
        default=os.environ.get("CARGO_TARGET_DIR", "target"),
    )
    args = ap.parse_args()

    baseline_path = pathlib.Path(args.baseline)
    target_dir = pathlib.Path(args.target_dir)

    if not baseline_path.exists():
        print(f"baseline {baseline_path} not found", file=sys.stderr)
        return 2

    base = load_baseline(baseline_path)
    tol_pct = float(base.get("tolerance_pct", 15))
    samples = base.get("samples", {})

    print(f"{'bench':<48}{'baseline':>14}{'observed':>14}{'delta_pct':>12}")
    regressions: list[str] = []
    missing: list[str] = []
    for bench, baseline_ns in samples.items():
        # Allow `_ns` suffix in baseline keys for clarity on bigger
        # numbers; the criterion path drops the suffix.
        bench_name = bench.removesuffix("_ns")
        observed = find_criterion_median(target_dir, bench_name)
        if observed is None:
            missing.append(bench_name)
            print(f"{bench_name:<48}{baseline_ns:>14.2f}{'<missing>':>14}{'':>12}")
            continue
        delta_pct = ((observed - baseline_ns) / baseline_ns) * 100.0
        marker = " !" if delta_pct > tol_pct else ""
        print(
            f"{bench_name:<48}{baseline_ns:>14.2f}{observed:>14.2f}{delta_pct:>11.2f}%{marker}"
        )
        if delta_pct > tol_pct:
            regressions.append(f"{bench_name}: +{delta_pct:.1f}% vs baseline")

    print()
    if missing:
        print(f"WARNING: {len(missing)} bench(es) missing — did you run `make bench`?")
        for m in missing:
            print(f"  - {m}")

    if regressions:
        print(f"FAIL: {len(regressions)} regression(s) above {tol_pct:.0f}% tolerance:")
        for r in regressions:
            print(f"  - {r}")
        return 1

    print(f"OK: all samples within {tol_pct:.0f}% of baseline.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
