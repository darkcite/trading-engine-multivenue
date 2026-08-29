# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6 (§1.6, D-8): hourly L2 depth digests BESIDE candles — the
research agent's offline view of the WS10-B top-K depth channel, in
the exact ``iv_digest`` pattern (same DB, same rolling-window upsert,
same manifest/anchor laws).

Per (venue, descriptor, 1h bucket), folded from every ``*-depth.pmlr``
in runs overlapping the window:

* ``imb_o/h/l/c`` — book imbalance ``(Σbid_qty − Σask_qty) /
  (Σbid_qty + Σask_qty)`` over the populated levels (the feature
  engine's DepthImb law, ÷1e9 to natural units);
* ``spread_bps_avg`` — mean top-of-book spread in bps of mid;
* ``near_notional_avg`` — mean Σ(px×qty) across BOTH sides' populated
  levels ÷1e12 (px 1e6 × qty 1e6; venue-NATIVE qty units — USD-ish on
  linear instruments, per docs/wire-format.md);
* ``n`` — snapshots folded.

STALE snapshots (flags bit 0 — resyncing book) and empty-side books
are SKIPPED and counted: a digest must never launder a known-broken
book into research statistics.

Wall-clock law (iv_digest): ``wall = run_epoch_ns + (ts_ns −
run_anchor_ns)``; manifest-less runs with depth records are skipped +
reported (pre-manifest capture).

Module surface only — never a worker verb; serialized like every
worker invocation (one SQLite writer)::

    python -m claude_worker.depth_digest              # rolling window
    python -m claude_worker.depth_digest --backfill   # whole root

Convention: full ``import x`` only.
"""

import argparse
import os
import pathlib
import sqlite3
import sys
import time
import typing

import claude_worker.features
import claude_worker.iv_digest
import claude_worker.pmlr

MS_1H: int = 3_600_000

#: Hourly only (the §1.6 plan cadence). ``tf`` stays a column so a
#: 1m lane can land additively (iv_digest schema symmetry).
TFS: dict[str, int] = {"1h": MS_1H}

WINDOW_H_ENV: str = "CLAUDE_WORKER_DEPTH_DIGEST_WINDOW_H"
WINDOW_H_DEFAULT: int = 26  # the candles §9.7 rolling-window default
DEFAULT_DB_PATH: str = "~/multivenue/worker/candles.db"
DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"

_SCHEMA: str = """
CREATE TABLE IF NOT EXISTS depth_digest (
  venue             INTEGER NOT NULL,
  descriptor        TEXT    NOT NULL,
  tf                TEXT    NOT NULL CHECK (tf IN ('1m','1h')),
  open_ts           INTEGER NOT NULL,
  imb_o REAL NOT NULL, imb_h REAL NOT NULL, imb_l REAL NOT NULL, imb_c REAL NOT NULL,
  spread_bps_avg    REAL    NOT NULL,
  near_notional_avg REAL    NOT NULL,
  n                 INTEGER NOT NULL,
  stale_skipped     INTEGER NOT NULL,
  computed_ts       INTEGER NOT NULL,
  PRIMARY KEY (venue, descriptor, tf, open_ts)
) WITHOUT ROWID;
"""


class Snapshot(typing.NamedTuple):
    """One in-flight (venue, descriptor, tf, bucket) aggregate.
    ``spread_sum``/``near_sum`` are running sums — divided by ``n`` at
    upsert."""

    imb_o: float
    imb_h: float
    imb_l: float
    imb_c: float
    spread_sum: float
    near_sum: float
    n: int
    stale_skipped: int


class FoldStats(typing.NamedTuple):
    """One fold pass's counters (report line)."""

    runs_seen: int
    runs_no_manifest: int
    runs_no_anchor: int
    records: int
    unresolved: int
    stale_skipped: int
    empty_skipped: int
    windowed_out: int


def measure(rec: claude_worker.pmlr.DepthRec) -> tuple[float, float, float] | None:
    """One snapshot → ``(imbalance, spread_bps, near_notional)`` or
    ``None`` for an unusable book (either side empty at the top).
    Mirrors the feature engine's on_depth laws (÷1e9 to natural
    units)."""
    bid0_px, bid0_qty = rec.bids[0]
    ask0_px, ask0_qty = rec.asks[0]
    if bid0_px <= 0 or ask0_px <= 0 or bid0_qty <= 0 or ask0_qty <= 0:
        return None
    bid_qty = 0
    ask_qty = 0
    near = 0
    for px, qty in rec.bids:
        if px <= 0 or qty <= 0:
            continue
        bid_qty += qty
        near += px * qty
    for px, qty in rec.asks:
        if px <= 0 or qty <= 0:
            continue
        ask_qty += qty
        near += px * qty
    total = bid_qty + ask_qty
    if total <= 0:
        return None
    imb = (bid_qty - ask_qty) / total
    mid = (bid0_px + ask0_px) / 2.0
    spread_bps = (ask0_px - bid0_px) / mid * 1e4
    return imb, spread_bps, near / 1e12


def fold_depth_snapshots(
    replay_root: pathlib.Path,
    lo_ms: int,
    now_ms: int,
    report: typing.Callable[[str], None],
) -> tuple[dict[tuple[int, str, str, int], Snapshot], FoldStats]:
    """Walk runs overlapping ``[lo, now)`` → per (venue, descriptor,
    tf, bucket) depth snapshot. Best-effort law: unreadable files are
    skipped, manifest-less runs with records are counted + reported."""
    out: dict[tuple[int, str, str, int], Snapshot] = {}
    runs_seen = 0
    runs_no_manifest = 0
    runs_no_anchor = 0
    records = 0
    unresolved = 0
    stale_skipped = 0
    empty_skipped = 0
    windowed_out = 0
    for run_dir in claude_worker.features.run_dirs(replay_root):
        try:
            epoch_ns = int(run_dir.name[len("run-") :])
        except ValueError:
            continue
        if epoch_ns // 1_000_000 + 36 * MS_1H < lo_ms:
            continue
        depth_paths = sorted(run_dir.glob("*-depth.pmlr"))
        depth_paths = [p for p in depth_paths if p.stat().st_size > claude_worker.pmlr.HEADER_SIZE]
        if not depth_paths:
            continue
        runs_seen += 1
        manifest = claude_worker.iv_digest.read_manifest(run_dir)
        anchor: int | None = None
        anchor_tried = False
        for path in depth_paths:
            try:
                with claude_worker.pmlr.DepthReader(path) as reader:
                    if len(reader) == 0:
                        continue
                    if manifest is None:
                        report(
                            f"depth-digest: {run_dir.name}: depth records but no"
                            " instrument manifest — run skipped (pre-manifest capture)"
                        )
                        runs_no_manifest += 1
                        break
                    if not anchor_tried:
                        anchor_tried = True
                        anchor = claude_worker.pmlr.run_anchor_ns(run_dir)
                        if anchor is None:
                            report(
                                f"depth-digest: {run_dir.name}: no readable tick file —"
                                " wall anchor unavailable, run skipped"
                            )
                            runs_no_anchor += 1
                    if anchor is None:
                        break
                    sym_map = manifest[0]
                    for rec in reader.records():
                        records += 1
                        if rec.flags & claude_worker.pmlr.DEPTH_FLAG_STALE:
                            stale_skipped += 1
                            continue
                        desc = sym_map.get((rec.venue, rec.sym))
                        if desc is None:
                            unresolved += 1
                            continue
                        wall_ms = (epoch_ns + (rec.ts_ns - anchor)) // 1_000_000
                        if wall_ms < lo_ms or wall_ms >= now_ms:
                            windowed_out += 1
                            continue
                        measured = measure(rec)
                        if measured is None:
                            empty_skipped += 1
                            continue
                        imb, spread_bps, near = measured
                        for tf, tf_ms in TFS.items():
                            bucket = wall_ms - (wall_ms % tf_ms)
                            key = (rec.venue, desc, tf, bucket)
                            snap = out.get(key)
                            if snap is None:
                                out[key] = Snapshot(imb, imb, imb, imb, spread_bps, near, 1, 0)
                            else:
                                out[key] = Snapshot(
                                    snap.imb_o,
                                    imb if imb > snap.imb_h else snap.imb_h,
                                    imb if imb < snap.imb_l else snap.imb_l,
                                    imb,
                                    snap.spread_sum + spread_bps,
                                    snap.near_sum + near,
                                    snap.n + 1,
                                    snap.stale_skipped,
                                )
            except (claude_worker.pmlr.PmlrError, OSError, ValueError):
                continue
    stats = FoldStats(
        runs_seen,
        runs_no_manifest,
        runs_no_anchor,
        records,
        unresolved,
        stale_skipped,
        empty_skipped,
        windowed_out,
    )
    return out, stats


def open_db(path: pathlib.Path) -> sqlite3.Connection:
    """Open (create) the DB with the candles-module pragmas."""
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA busy_timeout=5000")
    conn.executescript(_SCHEMA)
    return conn


def upsert_snapshots(
    conn: sqlite3.Connection,
    snaps: dict[tuple[int, str, str, int], Snapshot],
    now_ms: int,
) -> tuple[int, int, int]:
    """Insert/refresh the folded buckets → ``(inserted, refreshed,
    unchanged)``. A re-fold of a still-open bucket refreshes in place
    (rolling-window law); identical rows are left untouched."""
    inserted = 0
    refreshed = 0
    unchanged = 0
    for (venue, desc, tf, open_ts), snap in sorted(snaps.items()):
        spread_avg = snap.spread_sum / snap.n
        near_avg = snap.near_sum / snap.n
        row = conn.execute(
            "SELECT imb_o, imb_h, imb_l, imb_c, spread_bps_avg,"
            " near_notional_avg, n FROM depth_digest"
            " WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?",
            (venue, desc, tf, open_ts),
        ).fetchone()
        new = (snap.imb_o, snap.imb_h, snap.imb_l, snap.imb_c, spread_avg, near_avg, snap.n)
        if row is None:
            inserted += 1
        elif tuple(row) == new:
            unchanged += 1
            continue
        else:
            refreshed += 1
        conn.execute(
            "INSERT OR REPLACE INTO depth_digest"
            " (venue, descriptor, tf, open_ts, imb_o, imb_h, imb_l, imb_c,"
            "  spread_bps_avg, near_notional_avg, n, stale_skipped, computed_ts)"
            " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (venue, desc, tf, open_ts, *new, snap.stale_skipped, now_ms),
        )
    conn.commit()
    return inserted, refreshed, unchanged


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.depth_digest")
    parser.add_argument("--db", default=None)
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument("--window-h", type=int, default=None)
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    parser.add_argument(
        "--backfill",
        action="store_true",
        help="one-shot: fold the WHOLE replay root, not just the"
        " rolling window (operator-invoked)",
    )
    args = parser.parse_args(argv)
    env = os.environ
    db_path = pathlib.Path(
        args.db or env.get("CLAUDE_WORKER_CANDLES_DB", "") or DEFAULT_DB_PATH
    ).expanduser()
    replay_root = pathlib.Path(
        args.replay_dir or env.get("CLAUDE_WORKER_REPLAY_DIR", "") or DEFAULT_REPLAY_DIR
    ).expanduser()
    window_h = args.window_h or int(env.get(WINDOW_H_ENV, "") or WINDOW_H_DEFAULT)
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
    lo_ms = 0 if args.backfill else now_ms - window_h * MS_1H

    def report(line: str) -> None:
        print(line, file=sys.stderr)

    if not replay_root.is_dir():
        report(f"depth-digest: no replay root {replay_root} — nothing to fold")
        return 0
    snaps, stats = fold_depth_snapshots(replay_root, lo_ms, now_ms, report)
    conn = open_db(db_path)
    try:
        inserted, refreshed, unchanged = upsert_snapshots(conn, snaps, now_ms)
        total = conn.execute("SELECT count(*) FROM depth_digest").fetchone()[0]
    finally:
        conn.close()
    report(
        f"depth-digest: runs={stats.runs_seen} (no-manifest={stats.runs_no_manifest}"
        f" no-anchor={stats.runs_no_anchor}) records={stats.records}"
        f" unresolved={stats.unresolved} stale-skipped={stats.stale_skipped}"
        f" empty-skipped={stats.empty_skipped} windowed-out={stats.windowed_out}"
    )
    report(
        f"depth-digest: buckets={len(snaps)} +{inserted} refreshed={refreshed}"
        f" unchanged={unchanged} rows={total} db={db_path}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
