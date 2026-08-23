"""iv_digest — the §9.8 aggregated-IV snapshot table (M2 close).

A standalone MODULE (``python -m claude_worker.iv_digest``) — NOT a
verb: the 7-verb CLI surface is FROZEN (``cli.py`` untouched). One
pass = walk the capture (``<venue>-opt-summary.pmlr``, SlotKind 6,
layout pinned in docs/wire-format.md) → per (venue, descriptor) 1m and
1h IV snapshots → upsert into the ``iv_digest`` table BESIDE the
``candles`` table in the SAME worker-owned SQLite store
(``candles.db``; §9.9 keeps reading true: the strategist digest reads
"candles.db + feature files").

BINDING law (docs/mvp-completion-plan.md §9.8): "Aggregated IV
snapshots (per sym, 1m/1h) land in a table beside candles.db for the
strategist digest." Keying follows §9.4/§6: ``(venue, descriptor, tf,
open_ts)`` — NEVER bare SymbolId (options ordinals reshuffle per boot
BY DESIGN). Option syms resolve through the per-run
``options-manifest.tsv`` sidecar (M2 close, operator-ruled; format in
docs/wire-format.md): label→descriptor prefixes ``deribit:`` /
``okx:`` / ``binance-opt:`` + the venue instrument name. Runs without
a manifest but WITH opt-summary records are skipped and counted
(pre-manifest history is honestly unresolvable).

Snapshot semantics: per bucket — IV o/h/l/c (mark IV as a FRACTION;
wire ×1e9), record count ``n``, and last-in-bucket context columns:
``underlying_c`` (0 on the wire = absent → NULL), ``mark_px_c`` /
``oi_c`` only where the record ``flags`` say the venue supplied them
(bit0 mark_px, bit1 OI — OKX supplies neither in M2.3; BN adds
MARK_PX when its stream endpoint activates). Digest rows are a CACHE
derived from capture: they refresh freely when a re-fold changes them
(the candles ``derived`` convention; no conflict table — the PMLR
capture is the truth).

Wall mapping is the harness §3.3 law, exactly as the candles §9.7
capture lane: ``wall = epoch_ns + (ts − run_anchor)`` with the run
anchor = min first-ts across the run's readable tick files.

Constraints honored (M2-close kickoff): claude-worker is M3-owned —
this file + its test file are ADDITIVE ONLY. Hence two small local
mirrors, both flagged for fold-in at the next M3 window:
``SLOT_KIND_OPT_SUMMARY``/the kind-6 record decode (``pmlr.py`` does
not know kind 6 yet; header layout is REUSED from ``pmlr`` so there is
no second header definition) and ``_run_anchor_ns`` (mirrors
``candles._run_anchor_ns``).

Operational law: SERIALIZED against the hourly candles agent like
every worker invocation — ``pgrep -f claude-worker`` first, avoid the
top of the hour. The store is WAL and this module sets a busy timeout,
so an accidental overlap degrades to a wait, never corruption.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import mmap
import os
import pathlib
import sqlite3
import struct
import sys
import time
import typing

import claude_worker.features
import claude_worker.frames
import claude_worker.pmlr

# SlotKind 6 (docs/wire-format.md `OptSummary`) — pmlr.py stops at 5;
# fold-in candidate for the next M3 window.
SLOT_KIND_OPT_SUMMARY: int = 6

# The whole 64-byte slot is field-covered (no tail pad): ts u64 · sym
# u32 · venue u8 · flags u8 · pad2 · 4×i64 (mark_px_1e9, mark_iv_1e9,
# underlying_px_1e9, open_interest_1e6) · 4×i32 (delta_1e9, gamma_1e9,
# vega_1e6, theta_1e6).
_OPT: struct.Struct = struct.Struct("<QIBB2xqqqqiiii")
assert _OPT.size == claude_worker.pmlr.SLOT_SIZE

FLAG_MARK_PX: int = 1
FLAG_OPEN_INTEREST: int = 2

MS_1M: int = 60_000
MS_1H: int = 3_600_000
TFS: dict[str, int] = {"1m": MS_1M, "1h": MS_1H}

MANIFEST_FILE: str = "options-manifest.tsv"
# M4.2 ruling D3: the generalized manifest — EVERY allocated
# instrument, `<sym_u32>\t<descriptor>` (final §9.4 strings baked
# engine-side; venue derived from the SymbolId namespace byte).
# Preferred over MANIFEST_FILE, which stays one release for pre-D3
# runs (docs/wire-format.md "Instrument manifest").
INSTRUMENT_MANIFEST_FILE: str = "instrument-manifest.tsv"

# Manifest venue label → (frames venue id, descriptor prefix). The
# labels are the capture-file prefixes (wire-format.md); the
# descriptor prefixes follow the worker map-name convention (§9.4 —
# `deribit:BTC-27MAR26-100000-C`); Binance options take their own
# `binance-opt:` namespace beside `binance:` / `binance-usdm:`.
LABELS: dict[str, tuple[int, str]] = {
    "deribit": (claude_worker.frames.VENUE_DERIBIT, "deribit:"),
    "okx": (claude_worker.frames.VENUE_OKX, "okx:"),
    "bn": (claude_worker.frames.VENUE_BINANCE, "binance-opt:"),
}

WINDOW_H_ENV: str = "CLAUDE_WORKER_IV_DIGEST_WINDOW_H"
WINDOW_H_DEFAULT: int = 26  # the candles §9.7 rolling-window default
DEFAULT_DB_PATH: str = "~/multivenue/worker/candles.db"
DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"

_SCHEMA: str = """
CREATE TABLE IF NOT EXISTS iv_digest (
  venue        INTEGER NOT NULL,
  descriptor   TEXT    NOT NULL,
  tf           TEXT    NOT NULL CHECK (tf IN ('1m','1h')),
  open_ts      INTEGER NOT NULL,
  iv_o REAL NOT NULL, iv_h REAL NOT NULL, iv_l REAL NOT NULL, iv_c REAL NOT NULL,
  n            INTEGER NOT NULL,
  underlying_c REAL,
  mark_px_c    REAL,
  oi_c         REAL,
  computed_ts  INTEGER NOT NULL,
  PRIMARY KEY (venue, descriptor, tf, open_ts)
) WITHOUT ROWID;
"""


class OptRec(typing.NamedTuple):
    """One decoded `OptSummary` slot (docs/wire-format.md)."""

    ts_ns: int
    sym: int
    venue: int
    flags: int
    mark_px_1e9: int
    mark_iv_1e9: int
    underlying_px_1e9: int
    open_interest_1e6: int
    delta_1e9: int
    gamma_1e9: int
    vega_1e6: int
    theta_1e6: int


class OptReader:
    """Kind-6 sibling of ``claude_worker.pmlr.Reader`` (which refuses
    unknown slot kinds by design). Same container rules: mmap'd
    read-only, header REUSED from the pmlr reader (one layout, no
    drift), torn trailing slot tolerated (the engine may be
    mid-flush). Raises ``pmlr.PmlrError`` on a malformed container or
    a non-kind-6 file."""

    def __init__(self, path: pathlib.Path) -> None:
        self._file = path.open("rb")
        try:
            size = path.stat().st_size
            if size < claude_worker.pmlr.HEADER_SIZE:
                raise claude_worker.pmlr.PmlrError(
                    f"{path}: truncated before header ({size} B)"
                )
            self._map: mmap.mmap = mmap.mmap(self._file.fileno(), 0, access=mmap.ACCESS_READ)
        except BaseException:
            self._file.close()
            raise
        try:
            magic, version, slot_kind, epoch_ns = claude_worker.pmlr._HEADER.unpack_from(  # noqa: SLF001 — reader-defined layout, deliberately (craft.py precedent)
                self._map, 0
            )
            if magic != claude_worker.pmlr.MAGIC:
                raise claude_worker.pmlr.PmlrError(f"{path}: bad magic {magic!r}")
            if version > claude_worker.pmlr.VERSION_MAX:
                raise claude_worker.pmlr.PmlrError(
                    f"{path}: version {version} unsupported (max {claude_worker.pmlr.VERSION_MAX})"
                )
            if slot_kind != SLOT_KIND_OPT_SUMMARY:
                raise claude_worker.pmlr.PmlrError(
                    f"{path}: slot kind {slot_kind} is not OptSummary ({SLOT_KIND_OPT_SUMMARY})"
                )
        except BaseException:
            self.close()
            raise
        self.version: int = int(version)
        self.epoch_ns: int = int(epoch_ns)
        payload = size - claude_worker.pmlr.HEADER_SIZE
        self._count: int = payload // claude_worker.pmlr.SLOT_SIZE
        self.torn: bool = payload % claude_worker.pmlr.SLOT_SIZE != 0

    def close(self) -> None:
        """Unmap and close. Idempotent."""
        if hasattr(self, "_map"):
            self._map.close()
        self._file.close()

    def __enter__(self) -> "OptReader":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __len__(self) -> int:
        return self._count

    def record(self, index: int) -> OptRec:
        """Decode slot ``index`` (unpack_from straight off the map)."""
        if index < 0 or index >= self._count:
            raise IndexError(f"slot {index} out of range ({self._count} records)")
        offset = claude_worker.pmlr.HEADER_SIZE + index * claude_worker.pmlr.SLOT_SIZE
        return OptRec._make(_OPT.unpack_from(self._map, offset))

    def records(self) -> typing.Iterator[OptRec]:
        """All records, in file (= venue-thread time) order."""
        for i in range(self._count):
            yield self.record(i)


class Snapshot(typing.NamedTuple):
    """One in-progress (venue, descriptor, tf, bucket) aggregate."""

    iv_o: float
    iv_h: float
    iv_l: float
    iv_c: float
    n: int
    underlying_c: float | None
    mark_px_c: float | None
    oi_c: float | None


class FoldStats(typing.NamedTuple):
    """Fold accounting (report surface)."""

    runs_seen: int
    runs_no_manifest: int
    runs_no_anchor: int
    records: int
    unresolved_records: int
    malformed_manifest_lines: int
    windowed_out: int


def read_manifest(run_dir: pathlib.Path) -> tuple[dict[tuple[int, int], str], int] | None:
    """Resolve the run's sym→descriptor map, ``(venue_id, sym) →
    descriptor``. Prefers the D3 ``instrument-manifest.tsv`` (two
    tab-separated fields: decimal u32 sym + non-empty descriptor;
    venue = the SymbolId namespace byte, bits 31..24); falls back to
    the M2-close ``options-manifest.tsv`` (three fields: known label +
    sym + name, descriptor composed from the label prefix). ``None`` =
    neither file. Strict per line (labeling.py discipline): malformed
    lines are counted and skipped."""
    inst = run_dir / INSTRUMENT_MANIFEST_FILE
    if inst.is_file():
        try:
            text = inst.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            text = None
        if text is not None:
            out: dict[tuple[int, int], str] = {}
            malformed = 0
            for line in text.splitlines():
                if not line:
                    continue
                parts = line.split("\t")
                if len(parts) != 2:
                    malformed += 1
                    continue
                sym_s, descriptor = parts
                if not descriptor or not sym_s.isdigit():
                    malformed += 1
                    continue
                sym = int(sym_s)
                if sym <= 0 or sym > 0xFFFF_FFFF:
                    malformed += 1
                    continue
                out[(sym >> 24, sym)] = descriptor
            return out, malformed
    path = run_dir / MANIFEST_FILE
    if not path.is_file():
        return None
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None
    out = {}
    malformed = 0
    for line in text.splitlines():
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) != 3:
            malformed += 1
            continue
        label, sym_s, name = parts
        known = LABELS.get(label)
        if known is None or not name or not sym_s.isdigit():
            malformed += 1
            continue
        sym = int(sym_s)
        if sym <= 0 or sym > 0xFFFF_FFFF:
            malformed += 1
            continue
        venue, prefix = known
        out[(venue, sym)] = prefix + name
    return out, malformed


def _run_anchor_ns(run_dir: pathlib.Path) -> int | None:
    """The run's monotonic anchor — min first-ts across its readable
    tick files (harness §3.3 / monitor RunSpan law; mirrors
    ``candles._run_anchor_ns`` — fold-in candidate, next M3 window)."""
    anchor: int | None = None
    for path in sorted(run_dir.glob("*-ticks.pmlr")):
        try:
            with claude_worker.pmlr.Reader(path) as reader:
                if reader.slot_kind != claude_worker.pmlr.SLOT_KIND_TICK or len(reader) == 0:
                    continue
                first = reader.tick(0).ts_ns
        except (claude_worker.pmlr.PmlrError, OSError, ValueError):
            continue
        anchor = first if anchor is None else min(anchor, first)
    return anchor


def fold_iv_snapshots(
    replay_root: pathlib.Path,
    lo_ms: int,
    now_ms: int,
    report: typing.Callable[[str], None],
) -> tuple[dict[tuple[int, str, str, int], Snapshot], FoldStats]:
    """Walk runs overlapping ``[lo, now)`` → per (venue, descriptor,
    tf, bucket) IV snapshot. Best-effort law: unreadable files are
    skipped, manifest-less runs with records are counted + reported."""
    out: dict[tuple[int, str, str, int], Snapshot] = {}
    runs_seen = 0
    runs_no_manifest = 0
    runs_no_anchor = 0
    records = 0
    unresolved = 0
    malformed_total = 0
    windowed_out = 0
    for run_dir in claude_worker.features.run_dirs(replay_root):
        try:
            epoch_ns = int(run_dir.name[len("run-") :])
        except ValueError:
            continue
        # A run spans at most ~a day (daily restart); cheap skip for
        # runs that cannot reach the window (candles §9.7 pattern).
        if epoch_ns // 1_000_000 + 36 * MS_1H < lo_ms:
            continue
        opt_paths = [
            (label, run_dir / f"{label}-opt-summary.pmlr") for label in LABELS
        ]
        opt_paths = [(label, p) for (label, p) in opt_paths if p.is_file()]
        if not opt_paths:
            continue
        runs_seen += 1
        manifest = read_manifest(run_dir)
        if manifest is not None:
            malformed_total += manifest[1]
        anchor: int | None = None
        anchor_tried = False
        for _label, path in opt_paths:
            try:
                with OptReader(path) as reader:
                    if len(reader) == 0:
                        continue
                    if manifest is None:
                        report(
                            f"iv-digest: {run_dir.name}: opt-summary records but no"
                            f" {INSTRUMENT_MANIFEST_FILE} (or legacy {MANIFEST_FILE})"
                            " — run skipped (pre-manifest capture)"
                        )
                        runs_no_manifest += 1
                        break
                    if not anchor_tried:
                        anchor_tried = True
                        anchor = _run_anchor_ns(run_dir)
                        if anchor is None:
                            report(
                                f"iv-digest: {run_dir.name}: no readable tick file —"
                                " wall anchor unavailable, run skipped"
                            )
                            runs_no_anchor += 1
                    if anchor is None:
                        break
                    sym_map = manifest[0]
                    for rec in reader.records():
                        records += 1
                        desc = sym_map.get((rec.venue, rec.sym))
                        if desc is None:
                            unresolved += 1
                            continue
                        wall_ms = (epoch_ns + (rec.ts_ns - anchor)) // 1_000_000
                        if wall_ms < lo_ms or wall_ms >= now_ms:
                            windowed_out += 1
                            continue
                        iv = rec.mark_iv_1e9 / 1e9
                        underlying = (
                            rec.underlying_px_1e9 / 1e9
                            if rec.underlying_px_1e9 > 0
                            else None
                        )
                        mark_px = (
                            rec.mark_px_1e9 / 1e9
                            if rec.flags & FLAG_MARK_PX
                            else None
                        )
                        oi = (
                            rec.open_interest_1e6 / 1e6
                            if rec.flags & FLAG_OPEN_INTEREST
                            else None
                        )
                        for tf, tf_ms in TFS.items():
                            bucket = wall_ms - (wall_ms % tf_ms)
                            key = (rec.venue, desc, tf, bucket)
                            snap = out.get(key)
                            if snap is None:
                                out[key] = Snapshot(iv, iv, iv, iv, 1, underlying, mark_px, oi)
                            else:
                                out[key] = Snapshot(
                                    snap.iv_o,
                                    iv if iv > snap.iv_h else snap.iv_h,
                                    iv if iv < snap.iv_l else snap.iv_l,
                                    iv,
                                    snap.n + 1,
                                    underlying if underlying is not None else snap.underlying_c,
                                    mark_px if mark_px is not None else snap.mark_px_c,
                                    oi if oi is not None else snap.oi_c,
                                )
            except (claude_worker.pmlr.PmlrError, OSError, ValueError):
                continue
    stats = FoldStats(
        runs_seen,
        runs_no_manifest,
        runs_no_anchor,
        records,
        unresolved,
        malformed_total,
        windowed_out,
    )
    return out, stats


def open_db(path: pathlib.Path) -> sqlite3.Connection:
    """Open the SHARED worker store (candles.db) and ensure ONLY the
    ``iv_digest`` table exists — the candles tables are M3's schema and
    are never created or altered here (additive-files coordination
    law). WAL + busy timeout: an overlap with the hourly candles agent
    degrades to a bounded wait."""
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
    """Digest rows are a derived CACHE: insert new buckets, refresh
    rows a re-fold changed (a still-filling bucket at the previous
    pass), leave identical rows untouched. Returns (inserted,
    refreshed, unchanged)."""
    inserted = 0
    refreshed = 0
    unchanged = 0
    for (venue, desc, tf, bucket), snap in sorted(snaps.items()):
        row = conn.execute(
            "SELECT iv_o,iv_h,iv_l,iv_c,n,underlying_c,mark_px_c,oi_c FROM iv_digest"
            " WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?",
            (venue, desc, tf, bucket),
        ).fetchone()
        vals = (
            snap.iv_o,
            snap.iv_h,
            snap.iv_l,
            snap.iv_c,
            snap.n,
            snap.underlying_c,
            snap.mark_px_c,
            snap.oi_c,
        )
        if row is None:
            conn.execute(
                "INSERT INTO iv_digest"
                " (venue,descriptor,tf,open_ts,iv_o,iv_h,iv_l,iv_c,n,"
                "  underlying_c,mark_px_c,oi_c,computed_ts)"
                " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (venue, desc, tf, bucket, *vals, now_ms),
            )
            inserted += 1
        elif tuple(row) == vals:
            unchanged += 1
        else:
            conn.execute(
                "UPDATE iv_digest SET iv_o=?,iv_h=?,iv_l=?,iv_c=?,n=?,"
                " underlying_c=?,mark_px_c=?,oi_c=?,computed_ts=?"
                " WHERE venue=? AND descriptor=? AND tf=? AND open_ts=?",
                (*vals, now_ms, venue, desc, tf, bucket),
            )
            refreshed += 1
    conn.commit()
    return inserted, refreshed, unchanged


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.iv_digest")
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
        report(f"iv-digest: no replay root {replay_root} — nothing to fold")
        return 0
    snaps, stats = fold_iv_snapshots(replay_root, lo_ms, now_ms, report)
    conn = open_db(db_path)
    try:
        inserted, refreshed, unchanged = upsert_snapshots(conn, snaps, now_ms)
        total = conn.execute("SELECT count(*) FROM iv_digest").fetchone()[0]
    finally:
        conn.close()
    report(
        f"iv-digest: runs={stats.runs_seen} (no-manifest={stats.runs_no_manifest}"
        f" no-anchor={stats.runs_no_anchor}) records={stats.records}"
        f" unresolved={stats.unresolved_records}"
        f" manifest-malformed={stats.malformed_manifest_lines}"
        f" windowed-out={stats.windowed_out}"
    )
    report(
        f"iv-digest: buckets={len(snaps)} +{inserted} refreshed={refreshed}"
        f" unchanged={unchanged} rows={total} db={db_path}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
