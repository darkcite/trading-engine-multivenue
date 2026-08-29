# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6: the SEED LANE — warm the engine-resident VM after a restart
so cron-free strategies do not restart cold (D-1) or flat (D-2).

FundingSeed (kind 10, D-1)
    For every funding-capable descriptor in the NEWEST run's
    instrument manifest, push the funding table's settled prints from
    the last 73 h (72 h Apr72 window + 1 h slack) as the venue printed
    them: ``px = rate ×1e9 RAW`` — the ENGINE owns any ÷8 law via
    ``funding_print_divisor`` (deribit rows store ``interest_8h``
    verbatim; docs/vm2-plan.md §2 D-1), ``qty = venue print ts (ms)``.
    The engine dedups within half a venue period, so re-pushing an
    overlap is harmless by design.

PositionSeed (kind 11, D-2)
    Reconstruct open VM positions from the PREVIOUS run's
    ``engine-orders.pmlr`` (slot-5 = STRATEGY_SLOT_VM intents; paper
    law: an accepted intent IS the position advance) and re-seed them
    onto the CURRENT boot:

    * the committed ruleset artifact (``--ruleset``) gives row order —
      row index = position in the JSON ``rows`` array (the validator
      admits in file order; a committed artifact admitted fully);
      position rows are the rows carrying an ``exit`` key;
    * the (sym,ref)-unique-row LAW: reconstruction attributes orders
      by ACTION sym, so two position rows sharing an action descriptor
      are AMBIGUOUS — both are skipped + reported (the VM boots those
      rows Flat, D-2's designed fallback);
    * per row: fold the prev run's VM orders on the action sym
      chronologically (BID +qty, ASK −qty); a nonzero final net is an
      open position — side = net's sign, entry px = FIFO VWAP of the
      surviving entry basket (×1e6, the Order px scale), age =
      seconds since the LAST surviving entry (wall via the prev run's
      anchor law);
    * emit sym RE-RESOLVED through the CURRENT manifest (§1.4:
      ordinals reshuffle per boot); a descriptor absent from the
      current universe is skipped + reported;
    * ``ttl_ns = 0`` always — the engine drain EXPIRES any nonzero
      ttl on kind 11; qty carries AGE SECONDS, and entry quantity
      re-derives engine-side from the row's own sizing law.

Push protocol: one UDS connection, heartbeat FIRST (§5.4), then every
seed frame, close. Engine down ⇒ report + rc 1 (an hourly agent just
retries). ``--dry-run`` prints the frames and touches neither config
nor socket (works keyless).

Module surface only — never a worker verb; serialized like every
worker invocation::

    python -m claude_worker.seeds --dry-run
    python -m claude_worker.seeds --ruleset ~/multivenue/artifacts/rulesets/<hash>.json

Convention: full ``import x`` only.
"""

import argparse
import json
import os
import pathlib
import sqlite3
import sys
import time
import typing

import claude_worker.channel_map
import claude_worker.config
import claude_worker.features
import claude_worker.frames
import claude_worker.iv_digest
import claude_worker.pmlr
import claude_worker.state
import claude_worker.uds

MS_1H: int = 3_600_000
#: Funding look-back: the 72 h Apr72 window + 1 h slack.
FUNDING_WINDOW_H: int = 73
#: Engine-side per-sym print capacity (core-types FUNDING_BLOCKS
#: prints) — never push more than this per sym (newest kept).
MAX_PRINTS_PER_SYM: int = 640

DEFAULT_DB_PATH: str = "~/multivenue/worker/candles.db"
DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"

#: Descriptor prefix → funding-table venue id (frames.VENUE_*; the
#: candles/funding lane law).
_FUNDING_VENUE_OF_PREFIX: dict[str, int] = {
    "binance-usdm": claude_worker.frames.VENUE_BINANCE,
    "okx": claude_worker.frames.VENUE_OKX,
    "deribit": claude_worker.frames.VENUE_DERIBIT,
    "hyperliquid": claude_worker.frames.VENUE_HYPERLIQUID,
    "bybit-linear": claude_worker.frames.VENUE_BYBIT,
}


class SeedFrame(typing.NamedTuple):
    """One would-be AiCmd payload frame (the send_cmd keyword set)."""

    kind: int
    sym: int
    px: int
    qty: int
    side: int
    param_id: int


class FundingStats(typing.NamedTuple):
    """Funding-seed collection counters."""

    descriptors: int
    frames: int
    capped: int


class PositionStats(typing.NamedTuple):
    """Position-seed collection counters."""

    position_rows: int
    ambiguous: int
    flat: int
    unresolved_prev: int
    unresolved_cur: int
    seeded: int


def funding_seed_frames(
    conn: sqlite3.Connection,
    manifest: dict[tuple[int, int], str],
    now_ms: int,
) -> tuple[list[SeedFrame], FundingStats]:
    """Manifest × funding table → kind-10 frames, oldest print first
    per descriptor (the engine folds them into the same windows the
    live event lane feeds)."""
    frames: list[SeedFrame] = []
    descriptors = 0
    capped = 0
    lo_ms = now_ms - FUNDING_WINDOW_H * MS_1H
    for (_ns, sym), desc in sorted(manifest.items(), key=lambda item: item[0][1]):
        caps = claude_worker.channel_map.caps_of_descriptor(desc)
        if not caps & claude_worker.channel_map.CAP_FUNDING:
            continue
        prefix = desc.split(":", 1)[0]
        venue = _FUNDING_VENUE_OF_PREFIX.get(prefix)
        if venue is None:
            continue
        rows = conn.execute(
            "SELECT ts_ms, rate FROM funding"
            " WHERE venue=? AND descriptor=? AND ts_ms>=? AND ts_ms<?"
            " ORDER BY ts_ms",
            (venue, desc, lo_ms, now_ms),
        ).fetchall()
        if not rows:
            continue
        if len(rows) > MAX_PRINTS_PER_SYM:
            capped += 1
            rows = rows[-MAX_PRINTS_PER_SYM:]
        descriptors += 1
        for ts_ms, rate in rows:
            frames.append(
                SeedFrame(
                    kind=claude_worker.frames.KIND_FUNDING_SEED,
                    sym=sym,
                    px=round(rate * 1e9),
                    qty=ts_ms,
                    side=claude_worker.frames.SIDE_NONE,
                    param_id=0,
                )
            )
    return frames, FundingStats(descriptors, len(frames), capped)


class _Row(typing.NamedTuple):
    """One position row lifted from the ruleset artifact."""

    idx: int
    instrument: str


def position_rows_of_artifact(text: str) -> list[_Row] | None:
    """Artifact JSON → position rows (``exit`` key present), in row
    order. ``None`` = unusable artifact (not the validator's job to
    re-run here — a committed artifact already passed it)."""
    try:
        obj = json.loads(text)
    except ValueError:
        return None
    if not isinstance(obj, dict):
        return None
    rows = typing.cast(dict[str, object], obj).get("rows")
    if not isinstance(rows, list):
        return None
    out: list[_Row] = []
    for idx, row in enumerate(typing.cast(list[object], rows)):
        if not isinstance(row, dict):
            return None
        d = typing.cast(dict[str, object], row)
        if "exit" not in d:
            continue
        instrument = d.get("instrument")
        if not isinstance(instrument, str) or not instrument:
            return None
        out.append(_Row(idx, instrument))
    return out


class _Basket(typing.NamedTuple):
    """One reconstructed open position."""

    side: int
    vwap_px_1e6: int
    last_entry_ts_ns: int


def fold_vm_orders(
    orders: typing.Iterable[claude_worker.pmlr.OrderRec],
    sym: int,
) -> _Basket | None:
    """Chronological fold of one sym's VM intents → the surviving open
    position, or ``None`` when flat. FIFO reduce; a sign flip opens
    the residual as a fresh basket at the flipping order's px (module
    docs law)."""
    net = 0
    basket: list[tuple[int, int, int]] = []  # (px_1e6, qty_1e6, ts_ns)
    for order in orders:
        if order.sym != sym:
            continue
        if order.strategy_id != claude_worker.frames.STRATEGY_SLOT_VM:
            continue
        if order.side not in (claude_worker.frames.SIDE_BID, claude_worker.frames.SIDE_ASK):
            continue
        signed = order.qty if order.side == claude_worker.frames.SIDE_BID else -order.qty
        prev_net = net
        net += signed
        if net == 0:
            basket = []
        elif prev_net == 0 or (prev_net > 0) != (net > 0):
            basket = [(order.px, abs(net), order.ts_ns)]
        elif abs(net) > abs(prev_net):
            basket.append((order.px, abs(signed), order.ts_ns))
        else:
            reduce_left = abs(signed)
            while reduce_left > 0 and basket:
                px, qty, ts = basket[0]
                if qty > reduce_left:
                    basket[0] = (px, qty - reduce_left, ts)
                    reduce_left = 0
                else:
                    reduce_left -= qty
                    basket.pop(0)
    if net == 0 or not basket:
        return None
    total_qty = 0
    weighted = 0
    for px, qty, _ts in basket:
        total_qty += qty
        weighted += px * qty
    side = (
        claude_worker.frames.SIDE_BID if net > 0 else claude_worker.frames.SIDE_ASK
    )
    return _Basket(side, weighted // total_qty, basket[-1][2])


def position_seed_frames(
    rows: list[_Row],
    prev_run_dir: pathlib.Path,
    prev_manifest: dict[tuple[int, int], str],
    cur_manifest: dict[tuple[int, int], str],
    now_ns: int,
    report: typing.Callable[[str], None],
) -> tuple[list[SeedFrame], PositionStats]:
    """Position rows × prev-run intent log → kind-11 frames."""
    frames: list[SeedFrame] = []
    ambiguous_descs = {
        r.instrument
        for r in rows
        if sum(1 for other in rows if other.instrument == r.instrument) > 1
    }
    prev_by_desc = {desc: sym for (_ns, sym), desc in prev_manifest.items()}
    cur_by_desc = {desc: sym for (_ns, sym), desc in cur_manifest.items()}
    ambiguous = 0
    flat = 0
    unresolved_prev = 0
    unresolved_cur = 0
    orders_path = prev_run_dir / "engine-orders.pmlr"
    all_orders: list[claude_worker.pmlr.OrderRec] = []
    if orders_path.is_file():
        try:
            with claude_worker.pmlr.Reader(orders_path) as reader:
                if reader.slot_kind == claude_worker.pmlr.SLOT_KIND_ORDER:
                    all_orders = list(reader.orders())
        except (claude_worker.pmlr.PmlrError, OSError):
            all_orders = []
    prev_epoch_ns = int(prev_run_dir.name[len("run-") :])
    anchor = claude_worker.pmlr.run_anchor_ns(prev_run_dir)
    for row in rows:
        if row.instrument in ambiguous_descs:
            ambiguous += 1
            report(
                f"seeds: row {row.idx} ({row.instrument}): action descriptor"
                " shared by another position row — AMBIGUOUS, skipped"
                " (engine boots it Flat)"
            )
            continue
        prev_sym = prev_by_desc.get(row.instrument)
        if prev_sym is None:
            unresolved_prev += 1
            report(
                f"seeds: row {row.idx} ({row.instrument}): not in previous"
                " run's manifest — skipped"
            )
            continue
        basket = fold_vm_orders(all_orders, prev_sym)
        if basket is None:
            flat += 1
            continue
        cur_sym = cur_by_desc.get(row.instrument)
        if cur_sym is None:
            unresolved_cur += 1
            report(
                f"seeds: row {row.idx} ({row.instrument}): open position but"
                " descriptor absent from CURRENT universe — skipped"
            )
            continue
        if anchor is None:
            report(
                f"seeds: row {row.idx} ({row.instrument}): prev run has no"
                " wall anchor — age falls back to 0 s"
            )
            age_s = 0
        else:
            wall_ns = prev_epoch_ns + (basket.last_entry_ts_ns - anchor)
            age_s = max(0, (now_ns - wall_ns) // 1_000_000_000)
        frames.append(
            SeedFrame(
                kind=claude_worker.frames.KIND_POSITION_SEED,
                sym=cur_sym,
                px=basket.vwap_px_1e6,
                qty=age_s,
                side=basket.side,
                param_id=row.idx,
            )
        )
    stats = PositionStats(
        len(rows), ambiguous, flat, unresolved_prev, unresolved_cur, len(frames)
    )
    return frames, stats


def push_frames(frames: list[SeedFrame]) -> int:
    """Send the frames over ONE connection (heartbeat first, §5.4).
    Returns the count sent. Raises ``claude_worker.uds.UdsError`` when
    the engine is unreachable."""
    cfg = claude_worker.config.load_base_from_env()
    state = claude_worker.state.State(cfg.db_path)
    client = claude_worker.uds.UdsClient(
        cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state
    )
    client.connect()
    try:
        client.send_heartbeat()
        for f in frames:
            client.send_cmd(
                sym=f.sym,
                px=f.px,
                qty=f.qty,
                ttl_ns=0,
                kind=f.kind,
                venue=claude_worker.frames.VENUE_AI,
                strategy_id=claude_worker.frames.STRATEGY_SLOT_VM,
                side=f.side,
                param_id=f.param_id,
                flags=0,
            )
    finally:
        client.close()
    return len(frames)


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.seeds")
    parser.add_argument("--db", default=None)
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument(
        "--ruleset",
        default=None,
        help="committed ruleset artifact JSON — enables the"
        " PositionSeed lane (funding-only without it)",
    )
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print frames, send nothing (no config/socket needed)",
    )
    args = parser.parse_args(argv)
    env = os.environ
    db_path = pathlib.Path(
        args.db or env.get("CLAUDE_WORKER_CANDLES_DB", "") or DEFAULT_DB_PATH
    ).expanduser()
    replay_root = pathlib.Path(
        args.replay_dir or env.get("CLAUDE_WORKER_REPLAY_DIR", "") or DEFAULT_REPLAY_DIR
    ).expanduser()
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)

    def report(line: str) -> None:
        print(line, file=sys.stderr)

    runs = claude_worker.features.run_dirs(replay_root)
    if not runs:
        report(f"seeds: no run dirs under {replay_root} — nothing to seed")
        return 1
    cur_run = runs[-1]
    cur_manifest = claude_worker.iv_digest.read_manifest(cur_run)
    if cur_manifest is None:
        report(f"seeds: {cur_run.name}: no instrument manifest — cannot resolve syms")
        return 1

    frames: list[SeedFrame] = []
    if db_path.is_file():
        conn = sqlite3.connect(db_path)
        try:
            has_funding = (
                conn.execute(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name='funding'"
                ).fetchone()
                is not None
            )
            if has_funding:
                f_frames, f_stats = funding_seed_frames(conn, cur_manifest[0], now_ms)
                frames.extend(f_frames)
                report(
                    f"seeds: funding descriptors={f_stats.descriptors}"
                    f" frames={f_stats.frames} capped={f_stats.capped}"
                )
            else:
                report("seeds: no funding table — funding lane skipped")
        finally:
            conn.close()
    else:
        report(f"seeds: no candles db {db_path} — funding lane skipped")

    if args.ruleset is not None:
        artifact = pathlib.Path(args.ruleset).expanduser()
        try:
            text = artifact.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            report(f"seeds: ruleset {artifact}: {exc}")
            return 1
        rows = position_rows_of_artifact(text)
        if rows is None:
            report(f"seeds: ruleset {artifact}: unusable artifact JSON")
            return 1
        if rows and len(runs) < 2:
            report("seeds: no PREVIOUS run — position lane skipped (VM boots Flat)")
        elif rows:
            prev_run = runs[-2]
            prev_manifest = claude_worker.iv_digest.read_manifest(prev_run)
            if prev_manifest is None:
                report(
                    f"seeds: {prev_run.name}: no instrument manifest — position"
                    " lane skipped (VM boots Flat)"
                )
            else:
                p_frames, p_stats = position_seed_frames(
                    rows,
                    prev_run,
                    prev_manifest[0],
                    cur_manifest[0],
                    now_ms * 1_000_000,
                    report,
                )
                frames.extend(p_frames)
                report(
                    f"seeds: position rows={p_stats.position_rows}"
                    f" seeded={p_stats.seeded} flat={p_stats.flat}"
                    f" ambiguous={p_stats.ambiguous}"
                    f" unresolved-prev={p_stats.unresolved_prev}"
                    f" unresolved-cur={p_stats.unresolved_cur}"
                )

    if not frames:
        report("seeds: nothing to push")
        return 0
    if args.dry_run:
        for f in frames:
            print(
                f"kind={f.kind} sym={f.sym} px={f.px} qty={f.qty}"
                f" side={f.side} param_id={f.param_id}"
            )
        report(f"seeds: dry-run — {len(frames)} frames NOT sent")
        return 0
    try:
        sent = push_frames(frames)
    except claude_worker.uds.UdsError as exc:
        report(f"seeds: engine unreachable — {exc}")
        return 1
    report(f"seeds: sent {sent} frames")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
