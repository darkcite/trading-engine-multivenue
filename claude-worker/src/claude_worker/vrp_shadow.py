# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""vrp_shadow - reconcile the VRP member's OWN book against the harness.

The V8 acceptance instrument, and P1's (X1) live one. Two independent
accounts of the same campaign have to agree, or one of them is wrong:

* the ENGINE's, from the capture it wrote itself - `engine-orders.pmlr`
  (every order the engine accepted, M4.1) and `engine-fills.pmlr`
  (every fill it booked, now including the ones its own paper matcher
  modelled, X1), both attributed to the strategy slot that emitted
  them;
* the HARNESS's, from `audit-pnl` replaying those same logged intents
  through the offline fill model over the same capture.

They can disagree in exactly the ways that matter: a leg the engine
thought filled and the model refuses (or the reverse), a perp position
that drifted apart, a settlement value one of them never applied. Every
one of those is a position nobody is really tracking, which is what
made the F7 defect survive two live campaigns - the state file said
"long call, hedged" while the model held a naked short perp.

Exit 0 only when every leg agrees.

Offline tool - allocation is fine; never imported by the engine.
Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import collections
import json
import pathlib
import subprocess
import sys
import typing

import claude_worker.pmlr

#: The strategy-set slot the VRP member occupies.
SLOT_VRP: int = 1
#: `<sym>\t<descriptor>` — the manifest row shape.
_MANIFEST_FIELDS: int = 2
ORDERS_FILE: str = "engine-orders.pmlr"
FILLS_FILE: str = "engine-fills.pmlr"


class ShadowError(Exception):
    """The reconciliation could not be RUN (missing capture, audit-pnl
    refused). Distinct from a disagreement, which is a RESULT."""


class Leg(typing.NamedTuple):
    """One instrument's book, as one of the two accounts sees it."""

    fills: int
    pos_1e6: int


class Disagreement(typing.NamedTuple):
    """One leg the two accounts do not agree on."""

    sym: str
    field: str
    engine: int
    harness: int

    def line(self) -> str:
        return (
            f"vrp-shadow: DISAGREE {self.sym} {self.field}:"
            f" engine={self.engine} harness={self.harness}"
        )


def engine_legs(run_dir: pathlib.Path, slot: int = SLOT_VRP) -> dict[int, Leg]:
    """The member's own book per sym, from the fills the ENGINE booked.

    Only fills attributed to `slot` count. A fill carrying
    `STRATEGY_ID_NONE` is a venue fill fanned out to every member and
    belongs to none of them; before X1 every fill looked like that,
    which is exactly why the attribution bytes exist.
    """
    path = run_dir / FILLS_FILE
    if not path.is_file():
        raise ShadowError(f"{path} is absent - the run logged no fills at all")
    fills: collections.Counter[int] = collections.Counter()
    pos: dict[int, int] = {}
    with claude_worker.pmlr.Reader(path) as r:
        for f in r.fills():
            if f.strategy_id != slot:
                continue
            fills[f.sym] += 1
            signed = f.qty if f.side == 0 else -f.qty
            pos[f.sym] = pos.get(f.sym, 0) + signed
    return {sym: Leg(fills[sym], pos.get(sym, 0)) for sym in fills}


def engine_orders(run_dir: pathlib.Path, slot: int = SLOT_VRP) -> int:
    """How many orders the engine ACCEPTED for `slot` - the denominator
    the fill count is read against (an order that never filled is the
    F7 tell, not a missing record)."""
    path = run_dir / ORDERS_FILE
    if not path.is_file():
        raise ShadowError(f"{path} is absent - the run logged no orders at all")
    n = 0
    with claude_worker.pmlr.Reader(path) as r:
        for o in r.orders():
            if o.strategy_id == slot:
                n += 1
    return n


def run_audit_pnl(engine_bin: pathlib.Path, run_dir: pathlib.Path) -> tuple[dict, list[str]]:
    """`audit-pnl --dir <run>`: `(parsed stdout JSON, stderr lines)`.

    The per-sym detail lives on stderr by design - putting it in the
    JSON would change `audit-pnl`'s stdout on every root, and that
    stdout is one of the surfaces the option-model guard pins.
    """
    proc = subprocess.run(  # the engine binary, by explicit path
        [str(engine_bin), "audit-pnl", "--dir", str(run_dir)],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise ShadowError(f"audit-pnl exited {proc.returncode}: {proc.stderr.strip()[-400:]}")
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise ShadowError(f"audit-pnl stdout is not JSON: {exc}") from exc
    return report, proc.stderr.splitlines()


def harness_legs(stderr_lines: list[str], slot: int = SLOT_VRP) -> dict[str, Leg]:
    """The member's book per DESCRIPTOR, from `audit-pnl`'s per-sym rows.

    The rows follow their strategy's header and run until the next
    one, so the scope is the header - the descriptors themselves are
    not tagged.
    """
    # Keyed on the SLOT, never the label: the label is `audit-pnl`'s
    # own naming of the slot and is not this module's to assert.
    want = f"audit-pnl: strategy {slot} ("
    out: dict[str, Leg] = {}
    inside = False
    for raw in stderr_lines:
        line = raw.strip()
        if line.startswith("audit-pnl: strategy "):
            inside = line.startswith(want)
            continue
        if not inside or not line.startswith("audit-pnl:   "):
            continue
        body = line[len("audit-pnl:   ") :]
        if ": " not in body or " fills=" not in body:
            continue  # the ioc/ladder line, not a per-sym row
        # The SEPARATOR is ": " - a descriptor is `venue:name` and
        # carries a colon of its own, with no space after it.
        desc, rest = body.split(": ", 1)
        fields = dict(
            part.split("=", 1) for part in rest.strip().split(" ") if "=" in part
        )
        try:
            out[desc] = Leg(int(fields["fills"]), int(fields["pos"]))
        except (KeyError, ValueError):
            continue
    return out


def harness_strategy_row(report: dict, slot: int = SLOT_VRP) -> dict:
    """The `strategies[]` entry for `slot`, or an empty row."""
    for row in report.get("strategies", []):
        if row.get("strategy_id") == slot:
            return row
    return {}


def descriptor_table(run_dir: pathlib.Path) -> dict[int, str]:
    """`sym -> descriptor` from the run's own manifest, so the engine's
    dense syms can be named the way `audit-pnl` names them."""
    path = run_dir / "instrument-manifest.tsv"
    out: dict[int, str] = {}
    if not path.is_file():
        return out
    for raw in path.read_text(encoding="utf-8").splitlines():
        parts = raw.split("\t", 1)
        if len(parts) != _MANIFEST_FIELDS or not parts[1]:
            continue
        try:
            out[int(parts[0])] = parts[1].strip()
        except ValueError:
            continue
    return out


def reconcile(
    run_dir: pathlib.Path,
    engine_bin: pathlib.Path,
    slot: int = SLOT_VRP,
    report: typing.Callable[[str], None] | None = None,
) -> list[Disagreement]:
    """Compare the two accounts of one run; [] when every leg agrees."""
    say = report if report is not None else (lambda _line: None)
    names = descriptor_table(run_dir)
    eng_raw = engine_legs(run_dir, slot)
    eng: dict[str, Leg] = {}
    for sym, leg in eng_raw.items():
        eng[names.get(sym, f"sym-{sym:#010x}")] = leg
    n_orders = engine_orders(run_dir, slot)
    parsed, stderr_lines = run_audit_pnl(engine_bin, run_dir)
    har = harness_legs(stderr_lines, slot)
    row = harness_strategy_row(parsed, slot)

    say(
        f"vrp-shadow: engine orders={n_orders} fills={sum(x.fills for x in eng.values())}"
        f" legs={len(eng)}"
    )
    say(
        f"vrp-shadow: harness orders={row.get('orders', 0)} fills={row.get('fills', 0)}"
        f" legs={len(har)} opt_settled={row.get('opt_settled', 0)}"
    )

    out: list[Disagreement] = []
    for desc in sorted(set(eng) | set(har)):
        a = eng.get(desc, Leg(0, 0))
        b = har.get(desc, Leg(0, 0))
        if a.fills != b.fills:
            out.append(Disagreement(desc, "fills", a.fills, b.fills))
        if a.pos_1e6 != b.pos_1e6:
            out.append(Disagreement(desc, "pos_1e6", a.pos_1e6, b.pos_1e6))
    # The order count is the frame around the legs: the engine accepted
    # them, the harness replays them, and a gap means the capture and
    # the replay are not looking at the same campaign at all.
    if n_orders != int(row.get("orders", 0)):
        out.append(Disagreement("*", "orders", n_orders, int(row.get("orders", 0))))
    return out


def main(argv: list[str] | None = None) -> int:
    """CLI entry point: exit 0 only when every leg agrees."""
    ap = argparse.ArgumentParser(prog="claude_worker.vrp_shadow")
    ap.add_argument("--run", required=True, type=pathlib.Path, help="one run-<epoch_ns> directory")
    ap.add_argument(
        "--engine",
        required=True,
        type=pathlib.Path,
        help="the multivenue-engine binary to run audit-pnl with",
    )
    ap.add_argument("--slot", type=int, default=SLOT_VRP)
    args = ap.parse_args(argv)

    def say(line: str) -> None:
        print(line, file=sys.stderr)

    try:
        bad = reconcile(args.run, args.engine, args.slot, say)
    except ShadowError as exc:
        print(f"vrp-shadow: {exc}", file=sys.stderr)
        return 2
    for d in bad:
        print(d.line(), file=sys.stderr)
    if bad:
        print(f"vrp-shadow: {len(bad)} disagreement(s)")
        return 1
    print("vrp-shadow: every leg agrees")
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
