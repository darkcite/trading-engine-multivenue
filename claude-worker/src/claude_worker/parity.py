# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V8: the OFFLINE PARITY COMPARATOR — did the engine-resident VM
reproduce the cron carriers, from capture alone (audit doctrine)?

ONE ground truth for BOTH sides: ``engine-orders.pmlr`` (every order
the engine ACCEPTED, M4.1). The cron families push through the s4
Intent lane (``strategy_id == 4``, ai-exec); the VM emits as
``strategy_id == 5``. Same file, same clock, same manifests — no
cron-side state files are trusted (their state.json is overwritten
per cycle; capture is append-only).

Laws (docs/vm2-plan.md V8):

* EVENT law — fold each side's orders per (strategy, descriptor)
  chronologically (BID +qty, ASK −qty). An order that INCREASES |net|
  is an ENTRY leg, one that decreases it an EXIT leg (a sign flip is
  an exit + an entry). Every cron entry/exit event must have a VM
  event of the same type and net-direction on the same descriptor
  within the family's tolerance (one evaluation cadence, defaults
  ``--tol-xv-s 600`` / ``--tol-carry-s 7200``): matched, else MISS.
  VM events with no cron counterpart are EXTRA (informational — the
  VM legitimately trades rows the crons never carried; V8 GREEN cares
  about misses).
* POSITION law — at window end, per descriptor:
  sign(cron net) == sign(vm net) (either side flat-both-flat counts
  as agreement only when both are flat). Sizes are NOT compared —
  leg sizing deliberately differs (V7: $4,950 vs $9,900).
* P&L is NOT recomputed here — audit-pnl owns economics; this tool
  owns timing/direction parity. Run audit-pnl beside it for the
  per-strategy buckets.

Family attribution: cron tags are not in the Order slot, so families
split by DESCRIPTOR CLASS — ``xv`` = the mid-pair descriptors
(okx:BTC-USDT, binance:btcusdt, hyperliquid:BTC, binance-usdm:btcusdt),
``carry`` = everything else slot-4 touches (funding perps). Wall time
via the run anchor law (``pmlr.run_anchor_ns``).

Module surface only — never a worker verb::

    python -m claude_worker.parity --window-h 48

Convention: full ``import x`` only.
"""

import argparse
import os
import pathlib
import sys
import time
import typing

import claude_worker.features
import claude_worker.frames
import claude_worker.iv_digest
import claude_worker.pmlr

MS_1H: int = 3_600_000
WINDOW_H_DEFAULT: int = 48
TOL_XV_S_DEFAULT: int = 600
TOL_CARRY_S_DEFAULT: int = 7200
DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"

#: The xv family's descriptor set (V7 xv-v2 + the cron's pairs).
XV_DESCRIPTORS: frozenset[str] = frozenset(
    (
        "okx:BTC-USDT",
        "binance:btcusdt",
        "hyperliquid:BTC",
        "binance-usdm:btcusdt",
    )
)

ENTRY: str = "entry"
EXIT: str = "exit"


class Event(typing.NamedTuple):
    """One entry/exit leg event on one side."""

    wall_ns: int
    descriptor: str
    kind: str  # ENTRY | EXIT
    direction: int  # sign of the net AFTER an entry / BEFORE an exit
    strategy_id: int


class FamilyReport(typing.NamedTuple):
    """One family's comparison outcome."""

    cron_events: int
    vm_events: int
    matched: int
    misses: list[Event]
    extras: int
    position_disagreements: list[str]


def family_of(descriptor: str) -> str:
    """Descriptor → parity family (module docs)."""
    return "xv" if descriptor in XV_DESCRIPTORS else "carry"


def collect_orders(
    replay_root: pathlib.Path,
    lo_ms: int,
    now_ms: int,
    report: typing.Callable[[str], None],
) -> list[tuple[int, str, int, int, int]]:
    """Window's slot-4/slot-5 orders across runs →
    ``(wall_ns, descriptor, side, qty_1e6, strategy_id)``, wall-time
    ordered. Manifest-less or anchor-less runs are skipped +
    reported (audit doctrine: never guess an identity)."""
    out: list[tuple[int, str, int, int, int]] = []
    for run_dir in claude_worker.features.run_dirs(replay_root):
        try:
            epoch_ns = int(run_dir.name[len("run-") :])
        except ValueError:
            continue
        if epoch_ns // 1_000_000 + 36 * MS_1H < lo_ms:
            continue
        path = run_dir / "engine-orders.pmlr"
        if not path.is_file():
            continue
        manifest = claude_worker.iv_digest.read_manifest(run_dir)
        if manifest is None:
            report(f"parity: {run_dir.name}: no instrument manifest — run skipped")
            continue
        anchor = claude_worker.pmlr.run_anchor_ns(run_dir)
        if anchor is None:
            report(f"parity: {run_dir.name}: no wall anchor — run skipped")
            continue
        sym_map = manifest[0]
        try:
            with claude_worker.pmlr.Reader(path) as reader:
                if reader.slot_kind != claude_worker.pmlr.SLOT_KIND_ORDER:
                    continue
                for o in reader.orders():
                    if o.strategy_id not in (
                        claude_worker.frames.STRATEGY_SLOT_AI_EXEC,
                        claude_worker.frames.STRATEGY_SLOT_VM,
                    ):
                        continue
                    desc = sym_map.get((o.sym >> 24, o.sym))
                    if desc is None:
                        continue
                    wall_ns = epoch_ns + (o.ts_ns - anchor)
                    if wall_ns // 1_000_000 < lo_ms or wall_ns // 1_000_000 >= now_ms:
                        continue
                    out.append((wall_ns, desc, o.side, o.qty, o.strategy_id))
        except (claude_worker.pmlr.PmlrError, OSError):
            continue
    out.sort(key=lambda r: r[0])
    return out


def fold_events(
    orders: list[tuple[int, str, int, int, int]], strategy_id: int
) -> tuple[list[Event], dict[str, int]]:
    """One side's orders → entry/exit events + end-of-window net per
    descriptor (the EVENT law fold)."""
    net: dict[str, int] = {}
    events: list[Event] = []
    for wall_ns, desc, side, qty, sid in orders:
        if sid != strategy_id:
            continue
        if side == claude_worker.frames.SIDE_BID:
            signed = qty
        elif side == claude_worker.frames.SIDE_ASK:
            signed = -qty
        else:
            continue
        prev = net.get(desc, 0)
        cur = prev + signed
        net[desc] = cur
        if prev == 0:
            events.append(Event(wall_ns, desc, ENTRY, 1 if cur > 0 else -1, sid))
        elif (prev > 0) != (cur > 0) and cur != 0:
            # Sign flip: exit of the old direction + entry of the new.
            events.append(Event(wall_ns, desc, EXIT, 1 if prev > 0 else -1, sid))
            events.append(Event(wall_ns, desc, ENTRY, 1 if cur > 0 else -1, sid))
        elif cur == 0:
            events.append(Event(wall_ns, desc, EXIT, 1 if prev > 0 else -1, sid))
        elif abs(cur) > abs(prev):
            pass  # scale-in: same position, no new event (leg only)
        else:
            pass  # partial reduce: not a full exit — no event
    return events, net


def compare_family(
    cron_events: list[Event],
    vm_events: list[Event],
    cron_net: dict[str, int],
    vm_net: dict[str, int],
    tol_ns: int,
) -> FamilyReport:
    """The EVENT + POSITION laws for one family."""
    unmatched_vm = list(vm_events)
    matched = 0
    misses: list[Event] = []
    for ce in cron_events:
        hit = None
        for i, ve in enumerate(unmatched_vm):
            if (
                ve.descriptor == ce.descriptor
                and ve.kind == ce.kind
                and ve.direction == ce.direction
                and abs(ve.wall_ns - ce.wall_ns) <= tol_ns
            ):
                hit = i
                break
        if hit is None:
            misses.append(ce)
        else:
            unmatched_vm.pop(hit)
            matched += 1
    disagreements: list[str] = []
    for desc in sorted(set(cron_net) | set(vm_net)):
        c = cron_net.get(desc, 0)
        v = vm_net.get(desc, 0)
        c_sign = (c > 0) - (c < 0)
        v_sign = (v > 0) - (v < 0)
        if c_sign != v_sign:
            disagreements.append(f"{desc}: cron={c_sign:+d} vm={v_sign:+d}")
    return FamilyReport(
        len(cron_events),
        len(vm_events),
        matched,
        misses,
        len(unmatched_vm),
        disagreements,
    )


def split_by_family(
    events: list[Event], net: dict[str, int]
) -> dict[str, tuple[list[Event], dict[str, int]]]:
    out: dict[str, tuple[list[Event], dict[str, int]]] = {
        "xv": ([], {}),
        "carry": ([], {}),
    }
    for e in events:
        out[family_of(e.descriptor)][0].append(e)
    for desc, n in net.items():
        out[family_of(desc)][1][desc] = n
    return out


def render(family: str, r: FamilyReport, tol_s: int) -> list[str]:
    lines = [
        f"parity[{family}] tol={tol_s}s: cron-events={r.cron_events}"
        f" vm-events={r.vm_events} matched={r.matched} MISSES={len(r.misses)}"
        f" vm-extras={r.extras} position-disagreements={len(r.position_disagreements)}"
    ]
    for m in r.misses:
        lines.append(
            f"  MISS {family}: {m.kind} {m.descriptor} dir={m.direction:+d}"
            f" wall_ns={m.wall_ns} — no VM counterpart in ±{tol_s}s"
        )
    for d in r.position_disagreements:
        lines.append(f"  POSITION {family}: {d}")
    return lines


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.parity")
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument("--window-h", type=int, default=WINDOW_H_DEFAULT)
    parser.add_argument("--tol-xv-s", type=int, default=TOL_XV_S_DEFAULT)
    parser.add_argument("--tol-carry-s", type=int, default=TOL_CARRY_S_DEFAULT)
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    args = parser.parse_args(argv)
    env = os.environ
    replay_root = pathlib.Path(
        args.replay_dir or env.get("CLAUDE_WORKER_REPLAY_DIR", "") or DEFAULT_REPLAY_DIR
    ).expanduser()
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
    lo_ms = now_ms - args.window_h * MS_1H

    def report(line: str) -> None:
        print(line, file=sys.stderr)

    orders = collect_orders(replay_root, lo_ms, now_ms, report)
    cron_events, cron_net = fold_events(
        orders, claude_worker.frames.STRATEGY_SLOT_AI_EXEC
    )
    vm_events, vm_net = fold_events(orders, claude_worker.frames.STRATEGY_SLOT_VM)
    cron_fam = split_by_family(cron_events, cron_net)
    vm_fam = split_by_family(vm_events, vm_net)
    total_misses = 0
    total_disagreements = 0
    for family, tol_s in (("xv", args.tol_xv_s), ("carry", args.tol_carry_s)):
        r = compare_family(
            cron_fam[family][0],
            vm_fam[family][0],
            cron_fam[family][1],
            vm_fam[family][1],
            tol_s * 1_000_000_000,
        )
        for line in render(family, r, tol_s):
            print(line)
        total_misses += len(r.misses)
        total_disagreements += len(r.position_disagreements)
    print(
        f"parity: window-h={args.window_h} orders={len(orders)}"
        f" misses-total={total_misses}"
        f" position-disagreements-total={total_disagreements}"
        f" -> {'GREEN' if total_misses == 0 and total_disagreements == 0 else 'RED'}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
