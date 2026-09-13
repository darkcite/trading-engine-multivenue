# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15 calibration ledger — the O5 / G6.1 instrument (spec §7.3).

``backtest --member bin15 --emit-detail <json>`` writes a
``bin15_ledger`` block into the sidecar: one row per live instance per
30 s, carrying what the member BELIEVED (``p_hat_1e6``, and
``p_raw_1e6`` before recalibration) and, once the window can derive it,
what the instance actually PAID (``y``, 0 or 1e6). This module merges
those blocks across windows into one append-only file and reads the
calibration out of it.

``~/multivenue/worker/bin15/ledger.tsv`` — worker state, never git, and
never research: it is an accumulating measurement, and the FINDINGS it
supports belong in ``docs/research/outcome/`` like every other number.

**What this file can answer, and what it cannot.**

* **G6.1 (calibration ≤ 3 c per phase on ≥ 200 settled instances)** is
  exactly what the ledger is. A row is a belief with a realised
  outcome; bucketing by ``p_hat`` and comparing each bucket's mean
  belief to its realised rate IS the calibration table.
* **G6.2 (Arm A ≥ +2 c/contract)** and **G6.3 (Arm B paired model −
  null ≥ 0)** are P&L questions about FILLS, and the ledger carries no
  fills. They are read from the day report's per-strategy ``bin15``
  rows, which arrive by way of `audit_pnl`'s strategy label — not from
  here. A per-arm Brier score computed off these rows would look like
  an answer and would not be one: the member computes the SAME ``p_hat``
  under both arms (the arm changes what it quotes around, not what it
  believes), so the two arms' beliefs are the same distribution by
  construction.

Lanes (``python -m claude_worker.bin15_ledger <lane>``):

- ``merge --sidecar <path> [--sidecar <path> …] [--out <tsv>]`` — append
  the sidecars' ledger blocks, deduplicated by
  ``(ts_ns, family, outcome)``. Re-running one window is a no-op.
- ``calibration [--ledger <tsv>] [--min-instances N] [--buckets N]`` —
  the per-phase table and the G6.1 verdict.
- ``status [--ledger <tsv>]`` — rows, instances, settled instances.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import collections
import dataclasses
import json
import os
import pathlib
import sys
import typing

import claude_worker.bin15_ref

#: The default ledger, under the worker's own state root.
DEFAULT_LEDGER: str = "~/multivenue/worker/bin15/ledger.tsv"

#: The sidecar key the harness writes.
SIDECAR_KEY: str = "bin15_ledger"

#: `y` for a row whose instance the window could not settle.
Y_UNKNOWN: int = -1

#: Header; `#` lines are skipped on read.
HEADER: str = (
    "# bin15 ledger — one row per live instance per 30 s, merged from\n"
    "#   `backtest --member bin15 --emit-detail` sidecars.\n"
    "# ts_ns\tfamily\toutcome\ttau_ns\tp_hat_1e6\tp_raw_1e6\tarm\tentered\tmid_1e6\ty\n"
    "# y: 1000000 settled ITM, 0 settled OTM, -1 the window could not\n"
    "# derive the payout (its expiry or TWAP falls outside the window).\n"
    "# entered: 1 once the instance's $50 coverage entry was submitted, 0\n"
    "# before it, 0 for one the price bound declined to pay for (BIN15\n"
    "# P3/F6), -1 for a row written before the column existed. A row is an\n"
    "# OBSERVATION either way: G6.1 counts settled instances, not fills.\n"
    "# mid_1e6: the venue's own Yes mid at this instant, -1 when the book\n"
    "# was not two-sided. The skill gate is Brier(p_hat) < Brier(mid) at\n"
    "# the SAME instants, so the benchmark travels on the row.\n"
    "# Worker state. Never git; findings go to docs/research/outcome/.\n"
)

#: `entered` for a row written before the column existed (an 8-column
#: ledger, or a pre-P3 sidecar). Not 0: "we do not know" and "the member
#: declined to pay" are different facts, and a split by `entered` must
#: not quietly file the first as the second.
ENTERED_UNKNOWN: int = -1

#: `mid_1e6` for a row whose book was one-sided, or that predates the
#: column. A one-sided book has NO mid and inventing one would flatter
#: the benchmark the model is scored against.
MID_UNKNOWN: int = -1

#: G6.1's bound: 3 cents, ×1e6.
G61_MAX_ERR_1E6: int = 30_000

#: G6.1's sample floor.
G61_MIN_INSTANCES: int = 200

#: Calibration buckets over `p_hat` ∈ [0, 1].
BUCKETS_DEFAULT: int = 10

#: Phase names, indexed by `claude_worker.bin15_ref.phase_of`.
PHASE_NAMES: tuple[str, ...] = ("early", "mid", "late")


@dataclasses.dataclass(slots=True, frozen=True)
class Row:
    """One ledger row."""

    ts_ns: int
    family: int
    outcome: int
    tau_ns: int
    p_hat_1e6: int
    p_raw_1e6: int
    arm: int
    y: int
    entered: int = ENTERED_UNKNOWN
    mid_1e6: int = MID_UNKNOWN

    @property
    def key(self) -> tuple[int, int, int]:
        """Identity for deduplication: one sample per instance per instant."""
        return (self.ts_ns, self.family, self.outcome)

    @property
    def settled(self) -> bool:
        """Whether the window derived this instance's payout."""
        return self.y != Y_UNKNOWN

    def tsv(self) -> str:
        return (
            f"{self.ts_ns}\t{self.family}\t{self.outcome}\t{self.tau_ns}\t"
            f"{self.p_hat_1e6}\t{self.p_raw_1e6}\t{self.arm}\t{self.entered}\t"
            f"{self.mid_1e6}\t{self.y}\n"
        )


def ledger_path(path: str | None = None) -> pathlib.Path:
    """The ledger file, `~` expanded."""
    return pathlib.Path(os.path.expanduser(path or DEFAULT_LEDGER))


def rows_from_sidecar(text: str) -> list[Row]:
    """The `bin15_ledger` block of one `--emit-detail` sidecar.

    An empty list for a sidecar that has no block — every member other
    than bin15 writes none, and so does a bin15 run that never priced.
    """
    obj = json.loads(text)
    block = obj.get(SIDECAR_KEY)
    if not block:
        return []
    out: list[Row] = []
    for r in block:
        y = r.get("y")
        out.append(
            Row(
                ts_ns=int(r["ts_ns"]),
                family=int(r["family"]),
                outcome=int(r["outcome"]),
                tau_ns=int(r["tau_ns"]),
                p_hat_1e6=int(r["p_hat_1e6"]),
                p_raw_1e6=int(r["p_raw_1e6"]),
                arm=int(r.get("arm", 0)),
                y=Y_UNKNOWN if y is None else int(y),
                entered=int(r.get("entered", ENTERED_UNKNOWN)),
                mid_1e6=int(r.get("mid_1e6", MID_UNKNOWN)),
            )
        )
    return out


def read_ledger(path: pathlib.Path) -> list[Row]:
    """Every row in the ledger, oldest first. Empty when absent."""
    if not path.is_file():
        return []
    out: list[Row] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        f = s.split("\t")
        # 8 columns is a ledger written before BIN15 P3/P6 added
        # `entered` and `mid_1e6`; its rows are real observations and stay
        # readable, they simply do not know which of them were paid for or
        # what the venue was asking at the time.
        v = [int(x) for x in f]
        if len(f) == 8:
            out.append(
                Row(*v[:7], y=v[7], entered=ENTERED_UNKNOWN, mid_1e6=MID_UNKNOWN)
            )
            continue
        if len(f) != 10:
            raise ValueError(f"{path}: want 8 or 10 columns, got {len(f)}: {s!r}")
        out.append(Row(*v[:7], entered=v[7], mid_1e6=v[8], y=v[9]))
    return out


def merge(
    existing: typing.Sequence[Row], incoming: typing.Sequence[Row]
) -> tuple[list[Row], int]:
    """``(merged rows oldest first, rows added)``.

    Deduplicated by :attr:`Row.key`, EXISTING wins. That direction is
    deliberate: a re-cut window can carry a row whose `y` is still
    unknown because its expiry now falls outside the new cut, and
    letting it overwrite a settled row would quietly un-settle the
    ledger. A row that gains its `y` later arrives under a different
    window and a `y`-less duplicate of it is simply dropped.
    """
    by_key: dict[tuple[int, int, int], Row] = {r.key: r for r in existing}
    added = 0
    for r in incoming:
        if r.key in by_key:
            continue
        by_key[r.key] = r
        added += 1
    return sorted(by_key.values(), key=lambda r: (r.ts_ns, r.family, r.outcome)), added


def write_ledger(path: pathlib.Path, rows: typing.Sequence[Row]) -> None:
    """Atomic write (tmp + rename): a merge never leaves a torn file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(HEADER)
        for r in rows:
            f.write(r.tsv())
    os.replace(tmp, path)


@dataclasses.dataclass(slots=True)
class Bucket:
    """One calibration bucket: what was believed, what happened."""

    n: int = 0
    sum_p_1e6: int = 0
    sum_y_1e6: int = 0

    @property
    def mean_p_1e6(self) -> int:
        return self.sum_p_1e6 // self.n if self.n else 0

    @property
    def rate_1e6(self) -> int:
        return self.sum_y_1e6 // self.n if self.n else 0

    @property
    def err_1e6(self) -> int:
        return abs(self.mean_p_1e6 - self.rate_1e6)


@dataclasses.dataclass(slots=True)
class PhaseCalibration:
    """One phase's calibration table and its weighted error."""

    phase: int
    buckets: list[Bucket]
    rows: int = 0
    instances: int = 0

    @property
    def name(self) -> str:
        return PHASE_NAMES[self.phase]

    @property
    def weighted_err_1e6(self) -> int:
        """Σ n·|p̄ − ȳ| / Σ n — the number G6.1 bounds at 3 c."""
        if self.rows == 0:
            return 0
        acc = sum(b.n * b.err_1e6 for b in self.buckets)
        return acc // self.rows


def calibration(
    rows: typing.Sequence[Row], buckets: int = BUCKETS_DEFAULT
) -> list[PhaseCalibration]:
    """The per-phase calibration table over SETTLED rows only.

    An unsettled row carries no outcome to be calibrated against;
    counting it as a miss would make a window that simply ends early
    look like a model that is wrong.
    """
    if buckets < 1:
        raise ValueError(f"buckets must be positive, got {buckets}")
    out = [
        PhaseCalibration(phase=p, buckets=[Bucket() for _ in range(buckets)])
        for p in range(claude_worker.bin15_ref.PHASES)
    ]
    seen: list[set[int]] = [set() for _ in range(claude_worker.bin15_ref.PHASES)]
    for r in rows:
        if not r.settled:
            continue
        phase = claude_worker.bin15_ref.phase_of(r.tau_ns)
        p = min(max(r.p_hat_1e6, 0), 1_000_000)
        idx = min(p * buckets // 1_000_001, buckets - 1)
        b = out[phase].buckets[idx]
        b.n += 1
        b.sum_p_1e6 += p
        b.sum_y_1e6 += r.y
        out[phase].rows += 1
        seen[phase].add(r.outcome)
    for p in range(claude_worker.bin15_ref.PHASES):
        out[p].instances = len(seen[p])
    return out


def verdict(
    table: typing.Sequence[PhaseCalibration],
    max_err_1e6: int = G61_MAX_ERR_1E6,
    min_instances: int = G61_MIN_INSTANCES,
) -> tuple[str, list[str]]:
    """``(PASS | FAIL | INSUFFICIENT, reasons)`` for G6.1.

    INSUFFICIENT is NOT a failure and never waits: the gate opens when
    the evidence exists, and a phase that has not accrued its instances
    yet has not been measured. A phase with enough instances and an
    error over the bound is a FAIL, and one FAIL is the verdict.
    """
    reasons: list[str] = []
    short = False
    failed = False
    for t in table:
        if t.instances < min_instances:
            short = True
            reasons.append(
                f"{t.name}: {t.instances} settled instance(s) < {min_instances}"
            )
            continue
        if t.weighted_err_1e6 > max_err_1e6:
            failed = True
            reasons.append(
                f"{t.name}: calibration error {t.weighted_err_1e6 / 10_000:.2f} c "
                f"> {max_err_1e6 / 10_000:.2f} c"
            )
        else:
            reasons.append(
                f"{t.name}: calibration error {t.weighted_err_1e6 / 10_000:.2f} c "
                f"on {t.instances} instance(s)"
            )
    if failed:
        return "FAIL", reasons
    if short:
        return "INSUFFICIENT", reasons
    return "PASS", reasons


def render(table: typing.Sequence[PhaseCalibration]) -> str:
    """The human table."""
    out: list[str] = []
    for t in table:
        out.append(
            f"phase {t.name}: rows={t.rows} instances={t.instances} "
            f"weighted_err={t.weighted_err_1e6 / 10_000:.2f}c"
        )
        for i, b in enumerate(t.buckets):
            if b.n == 0:
                continue
            out.append(
                f"    [{i}] n={b.n} p_hat={b.mean_p_1e6 / 10_000:.2f}c "
                f"realised={b.rate_1e6 / 10_000:.2f}c err={b.err_1e6 / 10_000:.2f}c"
            )
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; the verb surface stays frozen once published."""
    ap = argparse.ArgumentParser(prog="claude_worker.bin15_ledger")
    sub = ap.add_subparsers(dest="lane", required=True)

    mg = sub.add_parser("merge", help="append sidecar ledger blocks")
    mg.add_argument("--sidecar", action="append", required=True, type=pathlib.Path)
    mg.add_argument("--out", default=None)

    cal = sub.add_parser("calibration", help="the per-phase table + the G6.1 verdict")
    cal.add_argument("--ledger", default=None)
    cal.add_argument("--buckets", type=int, default=BUCKETS_DEFAULT)
    cal.add_argument("--min-instances", type=int, default=G61_MIN_INSTANCES)
    cal.add_argument("--max-err-1e6", type=int, default=G61_MAX_ERR_1E6)

    st = sub.add_parser("status", help="rows, instances, settled instances")
    st.add_argument("--ledger", default=None)

    args = ap.parse_args(argv)

    if args.lane == "merge":
        path = ledger_path(args.out)
        incoming: list[Row] = []
        for p in args.sidecar:
            if not p.is_file():
                print(f"bin15-ledger: {p}: no such file", file=sys.stderr)
                return 2
            incoming.extend(rows_from_sidecar(p.read_text(encoding="utf-8")))
        merged, added = merge(read_ledger(path), incoming)
        write_ledger(path, merged)
        settled = sum(1 for r in merged if r.settled)
        print(
            f"bin15-ledger: +{added} row(s) from {len(args.sidecar)} sidecar(s) -> "
            f"{path} ({len(merged)} rows, {settled} settled)"
        )
        return 0

    path = ledger_path(args.ledger)
    rows = read_ledger(path)
    if args.lane == "status":
        instances = collections.Counter(r.outcome for r in rows)
        settled = {r.outcome for r in rows if r.settled}
        # BIN15 P3.3 (F6): the two populations, side by side. An
        # instance is ENTERED if any of its rows says so — the flag goes
        # up mid-instance and never comes down.
        entered = {r.outcome for r in rows if r.entered == 1}
        unknown = {r.outcome for r in rows if r.entered == ENTERED_UNKNOWN}
        print(
            f"bin15-ledger: {path} rows={len(rows)} instances={len(instances)} "
            f"settled_instances={len(settled)} families={len({r.family for r in rows})} "
            f"entered_instances={len(entered)} "
            f"settled_and_entered={len(settled & entered)} "
            f"entered_unknown={len(unknown)}"
        )
        return 0

    table = calibration(rows, args.buckets)
    print(render(table))
    v, reasons = verdict(table, args.max_err_1e6, args.min_instances)
    for r in reasons:
        print(f"  {r}")
    print(f"G6.1: {v}")
    # Exit 0 ONLY on PASS, the regime-soak law: a gate that exits 0 on
    # "not measured yet" is a gate a script can pass by not measuring.
    return 0 if v == "PASS" else 1


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
