# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""window_root — cut a capture run into ≤ 2 h replay windows (ICDP I6).

The capture-window law (docs/venue-time-capture-plan.md §6.1, operator
ruling 2026-09-03): no replay window may exceed 2 hours, and whole-root
replays OOM anyway (CLAUDE.md ops debt c). This module turns one
``run-<epoch_ns>/`` directory into bounded windows the harness accepts
as runs of their own:

* every ``*.pmlr`` file is sliced to the slots whose ``ts_ns`` (offset 0
  of every slot kind, docs/wire-format.md) lies in ``[lo, hi)`` — files
  are per-thread monotone, so the slice is two binary searches on an
  mmap plus ONE byte-range copy (stdlib only, no per-slot Python loop);
* the 64-byte header is copied with ``epoch_ns`` advanced by the
  window's offset from the run's first tick, and the window directory is
  named ``run-<advanced epoch>`` — the harness's directory/epoch check
  holds and its wall rebase (``wall = epoch + (ts - ts_first)``) lands
  on the true wall clock, so consecutive windows are disjoint runs on
  its virtual timeline;
* the manifests are copied;
* RG3: ``ai-cmds.pmlr`` is cut by ``ts_ns`` like every file (the
  events-file lesson — an uncut file drags out-of-window rows into the
  merge), PLUS the latest ``SetRegime`` frame per profile stamped BEFORE
  the window whose TTL still covers the window's first instant is
  carried over in front of the slice — the declaration that was in
  force at the cut. The harness clamps such a frame to its first tick
  and shortens the TTL by the clamp, so the carried declaration expires
  on the true wall clock.
* the harness FUNDING seed (RG4's carry blocker, 2026-09-05): with a
  ``candles.db`` the cut writes ``funding-seed.tsv`` — the window's
  manifest × the funding table, the boot seed lane's own law
  (``claude_worker.seeds.funding_seed_rows``: 73 h before the window's
  first instant) — so a ≤ 2 h window replays an ``apr24``/``apr72`` row
  warm from its first tick exactly as the live engine is.

Offline tool — allocation is fine; never imported by the engine.
Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import mmap
import os
import pathlib
import shutil
import sqlite3
import struct
import sys
import time
import typing

import claude_worker.iv_digest
import claude_worker.regime
import claude_worker.seeds
import claude_worker.vrp_seed

HEADER_SIZE: int = 64
SEED_FILE: str = "regime-seed.tsv"
_HEADER = struct.Struct("<4sHBxQ")
SLOT_KIND_AI_CMD: int = 4
SLOT_KIND_DEPTH: int = 7
DEPTH_SLOT: int = 192
SLOT: int = 64
WINDOW_MAX_S: float = 2 * 3600.0
MANIFESTS: tuple[str, ...] = ("instrument-manifest.tsv", "options-manifest.tsv")
# AiCmd slot (docs/wire-format.md §3): ts_ns @0, ttl_ns @32, kind @40,
# param_id @44 — the three fields the carry-over law reads.
_AI_TTL_OFF: int = 32
_AI_KIND_OFF: int = 40
_AI_PARAM_OFF: int = 44
KIND_SET_REGIME: int = 12
REGIME_PROFILES: int = 2
# How far back the carry-over scans for a live declaration (frames): the
# AI plane is low-cadence (heartbeats every few seconds), a 15-minute
# TTL lies within a few hundred frames — 4096 is a generous ceiling.
_CARRY_SCAN_MAX: int = 4096


class WindowError(Exception):
    """A run that cannot be windowed (bad header, no ticks, over-long window)."""


def _header(path: pathlib.Path) -> tuple[int, int, int]:
    """(version, kind, epoch_ns) of a PMLR file."""
    with path.open("rb") as f:
        head = f.read(HEADER_SIZE)
    if len(head) < HEADER_SIZE:
        raise WindowError(f"{path}: short header")
    magic, version, kind, epoch = _HEADER.unpack_from(head, 0)
    if magic != b"PMLR":
        raise WindowError(f"{path}: bad magic")
    return version, kind, epoch


def _slot_size(kind: int) -> int:
    return DEPTH_SLOT if kind == SLOT_KIND_DEPTH else SLOT


def _ts_at(mm: mmap.mmap, i: int, slot: int) -> int:
    return struct.unpack_from("<Q", mm, HEADER_SIZE + i * slot)[0]


def _lower_bound(mm: mmap.mmap, n: int, slot: int, ts: int) -> int:
    """First slot index with ``ts_ns >= ts`` (file monotone in ts)."""
    lo, hi = 0, n
    while lo < hi:
        mid = (lo + hi) // 2
        if _ts_at(mm, mid, slot) < ts:
            lo = mid + 1
        else:
            hi = mid
    return lo


def first_last_ts(path: pathlib.Path) -> tuple[int, int] | None:
    """(first, last) ``ts_ns`` of a PMLR file; None when it holds no slot."""
    size = os.path.getsize(path)
    _, kind, _ = _header(path)
    slot = _slot_size(kind)
    n = (size - HEADER_SIZE) // slot
    if n <= 0:
        return None
    with path.open("rb") as f:
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            return _ts_at(mm, 0, slot), _ts_at(mm, n - 1, slot)
        finally:
            mm.close()


def run_span(run_dir: pathlib.Path) -> tuple[int, int] | None:
    """(min first ts over TICK files, max last ts over every file) — the
    harness's anchor law for the first; None when the run has no tick."""
    first = None
    last = None
    for p in sorted(run_dir.glob("*.pmlr")):
        fl = first_last_ts(p)
        if fl is None:
            continue
        if p.name.endswith("-ticks.pmlr"):
            first = fl[0] if first is None else min(first, fl[0])
        last = fl[1] if last is None else max(last, fl[1])
    if first is None or last is None:
        return None
    return first, last


def windows_of(run_dir: pathlib.Path, window_s: float = WINDOW_MAX_S) -> list[tuple[float, float]]:
    """Consecutive ``(from_s, to_s)`` windows (seconds after the run's first
    tick) covering the run, each ≤ ``window_s``; [] when the run has no tick."""
    if window_s <= 0 or window_s > WINDOW_MAX_S:
        raise WindowError(f"window {window_s} s violates the 2 h law")
    span = run_span(run_dir)
    if span is None:
        return []
    total_s = (span[1] - span[0]) / 1e9
    out: list[tuple[float, float]] = []
    start = 0.0
    while start <= total_s:
        out.append((start, start + window_s))
        start += window_s
    return out


def _carry_set_regime(mm: mmap.mmap, a: int, lo_ts: int) -> list[bytes]:
    """RG3: per profile, the LATEST ``SetRegime`` frame before slot ``a``
    decides (a later declaration replaces an earlier one in the engine);
    it is carried iff ``ts + ttl > lo_ts`` — still in force at the
    window's first instant. Returned in file (ts) order."""
    seen: set[int] = set()
    found: dict[int, bytes] = {}
    i = a - 1
    stop = max(0, a - _CARRY_SCAN_MAX)
    while i >= stop and len(seen) < REGIME_PROFILES:
        off = HEADER_SIZE + i * SLOT
        if mm[off + _AI_KIND_OFF] == KIND_SET_REGIME:
            profile = struct.unpack_from("<H", mm, off + _AI_PARAM_OFF)[0]
            if profile < REGIME_PROFILES and profile not in seen:
                seen.add(profile)
                ts, ttl = _ts_at(mm, i, SLOT), struct.unpack_from("<Q", mm, off + _AI_TTL_OFF)[0]
                if ts + ttl > lo_ts:
                    found[profile] = bytes(mm[off : off + SLOT])
        i -= 1
    return [found[p] for p in sorted(found, key=lambda p: _ts_at_bytes(found[p]))]


def _ts_at_bytes(slot_bytes: bytes) -> int:
    return struct.unpack_from("<Q", slot_bytes, 0)[0]


def _cut_file(src: pathlib.Path, dst: pathlib.Path, lo_ts: int, hi_ts: int, epoch_ns: int) -> tuple[int, int]:
    """Copy the slots with ``lo_ts <= ts < hi_ts``; header epoch rewritten.
    An ``ai-cmds.pmlr`` additionally carries the pre-window ``SetRegime``
    declarations still in force at ``lo_ts`` (module docs)."""
    size = os.path.getsize(src)
    _, kind, _ = _header(src)
    slot = _slot_size(kind)
    n = (size - HEADER_SIZE) // slot  # a torn trailing slot is dropped
    with src.open("rb") as f:
        head = bytearray(f.read(HEADER_SIZE))
        struct.pack_into("<Q", head, 8, epoch_ns)
        if n == 0:
            dst.write_bytes(bytes(head))
            return 0, 0
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            a = _lower_bound(mm, n, slot, lo_ts)
            b = _lower_bound(mm, n, slot, hi_ts)
            carried = _carry_set_regime(mm, a, lo_ts) if kind == SLOT_KIND_AI_CMD else []
            with dst.open("wb") as out:
                out.write(bytes(head))
                for frame in carried:
                    out.write(frame)
                if b > a:
                    out.write(mm[HEADER_SIZE + a * slot : HEADER_SIZE + b * slot])
        finally:
            mm.close()
    return n, max(b - a, 0) + len(carried)


def cut_run(
    run_dir: pathlib.Path,
    dst_root: pathlib.Path,
    from_s: float,
    to_s: float,
    report: typing.Callable[[str], None] | None = None,
    seed: tuple[pathlib.Path, pathlib.Path] | None = None,
    vrp: pathlib.Path | None = None,
) -> pathlib.Path:
    """Materialise the window ``[from_s, to_s)`` of ``run_dir`` under
    ``dst_root`` as ``run-<epoch + from_s>``; returns the new run dir.

    RG3 (plan §4.3): ``seed = (regime.toml, candles.db)`` writes the
    window's own ``regime-seed.tsv`` — the ``REGIME_RING_MIN`` minute
    closes BEFORE the window's first instant, from ``candles.db`` (derived
    data, not a capture window) — which the harness picks up by default
    so a ≤ 2 h window can warm the detector's 4 h profile. Either file
    absent ⇒ no seed (the harness warms live and says so). The same
    ``candles.db`` (its funding table) writes ``funding-seed.tsv`` for
    the window's manifest — regime.toml is not needed for that one.

    VRP V5: ``vrp`` = a ``vrp.toml`` path also writes the window's own
    ``vrp-seed.tsv`` from that same ``candles.db`` — the 60+ settled
    ``(x, y)`` pairs the VRP member needs before it will produce a
    bound. Seeded AS OF the window's first instant, never the wall
    clock: a window cut from last month must replay only what was known
    then, or the harness fits on the future. Absent file ⇒ no seed, and
    the member holds."""
    if to_s <= from_s or to_s - from_s > WINDOW_MAX_S:
        raise WindowError(f"window {from_s}..{to_s} s violates the 2 h law")
    span = run_span(run_dir)
    if span is None:
        raise WindowError(f"{run_dir}: no tick to anchor a window")
    try:
        epoch_ns = int(run_dir.name[4:])
    except ValueError as exc:
        raise WindowError(f"{run_dir}: not a run-<epoch_ns> directory") from exc
    lo_ts = span[0] + int(from_s * 1e9)
    hi_ts = span[0] + int(to_s * 1e9)
    new_epoch = epoch_ns + int(from_s * 1e9)
    out_dir = dst_root / f"run-{new_epoch}"
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    for m in MANIFESTS:
        if (run_dir / m).is_file():
            shutil.copyfile(run_dir / m, out_dir / m)
    for p in sorted(run_dir.glob("*.pmlr")):
        total, kept = _cut_file(p, out_dir / p.name, lo_ts, hi_ts, new_epoch)
        if report is not None:
            report(f"window-root: {run_dir.name} {p.name} {total} -> {kept} ({from_s:.0f}..{to_s:.0f} s)")
    if seed is not None and seed[0].is_file() and seed[1].is_file():
        n_desc, n_rows = claude_worker.regime.seed_out(
            seed[0],
            seed[1],
            out_dir / SEED_FILE,
            claude_worker.regime.SEED_MINUTES_DEFAULT,
            now_ms=new_epoch // 1_000_000,
        )
        if report is not None:
            report(f"window-root: {run_dir.name} {SEED_FILE} {n_rows} rows for {n_desc} descriptors")
    if seed is not None and seed[1].is_file():
        n_desc, n_rows = funding_seed_out(out_dir, seed[1], new_epoch // 1_000_000)
        if report is not None:
            report(
                f"window-root: {run_dir.name} {claude_worker.seeds.FUNDING_SEED_FILE}"
                f" {n_rows} prints for {n_desc} descriptors"
            )
    if vrp is not None and vrp.is_file() and seed is not None and seed[1].is_file():
        n_pairs, vrp_stats = claude_worker.vrp_seed.seed_for_window(
            out_dir, seed[1], vrp, new_epoch // 1_000_000
        )
        if report is not None:
            report(
                f"window-root: {run_dir.name} {claude_worker.vrp_seed.SEED_FILE}"
                f" {n_pairs} pair(s) (considered={vrp_stats.expiries_considered}"
                f" unsettled={vrp_stats.skipped_unsettled})"
            )
    return out_dir


def funding_seed_out(run_dir: pathlib.Path, db_path: pathlib.Path, now_ms: int) -> tuple[int, int]:
    """Write ``run_dir/funding-seed.tsv`` from the run's own manifest and
    the funding table (prints before ``now_ms``); returns
    ``(descriptors, prints)``. No manifest, no funding table or no print
    ⇒ nothing written, ``(0, 0)`` — the harness then warms from the
    window and its summary says ``warmup=table``."""
    manifest = claude_worker.iv_digest.read_manifest(run_dir)
    if manifest is None:
        return 0, 0
    conn = sqlite3.connect(db_path)
    try:
        if not claude_worker.seeds.has_funding_table(conn):
            return 0, 0
        rows, stats = claude_worker.seeds.funding_seed_rows(conn, manifest[0], now_ms)
    finally:
        conn.close()
    if not rows:
        return 0, 0
    out = run_dir / claude_worker.seeds.FUNDING_SEED_FILE
    claude_worker.seeds.write_funding_seed_tsv(out, rows)
    return stats.descriptors, stats.frames


# ---- RG4: the standing window POOL (plan §5.3 gate shape) ----
#
# The composer's gate runs on N ≥ 4 pooled DISJOINT ≤ 2 h seeded windows
# that already exist. The nightly lane deletes its cuts, so the pool is
# its own directory of `run-<epoch>` cuts, filled newest-first from the
# capture runs and pruned by WINDOW COUNT — never by a time: the
# operator's 2026-09-05 ruling forbids any test / soak / protect time
# beyond 2 hours, so "keep the last K windows" is the only retention law
# this pool knows. A window is admitted only when the run holds it in
# full (the growing tail of the live run waits for its next cut).

POOL_DIRNAME: str = "windows"
POOL_SIZE_DEFAULT: int = 8
#: PMLR versions below this are stale-BLIND (no venue time — pitfall 17);
#: the pool refuses them so every evidence row is a judged one.
POOL_MIN_PMLR_VERSION: int = 3


def pool_dir_for(db_path: pathlib.Path) -> pathlib.Path:
    """``<worker dir>/windows`` beside ``state.db`` (the ``regime_dir_for``
    precedent — no new env key)."""
    return db_path.parent / POOL_DIRNAME


def complete_windows(run_dir: pathlib.Path, window_s: float = WINDOW_MAX_S) -> list[tuple[float, float]]:
    """The FULL ``window_s`` slices a run holds (``windows_of`` minus the
    partial tail) — the only windows a pool may cut."""
    span = run_span(run_dir)
    if span is None:
        return []
    total_s = (span[1] - span[0]) / 1e9
    return [w for w in windows_of(run_dir, window_s) if w[1] <= total_s]


def run_pmlr_version(run_dir: pathlib.Path) -> int | None:
    """The PMLR version of the run's tick files (the minimum over them);
    None when the run has no tick file."""
    version: int | None = None
    for p in sorted(run_dir.glob("*-ticks.pmlr")):
        try:
            v, _kind, _epoch = _header(p)
        except WindowError:
            continue
        version = v if version is None else min(version, v)
    return version


class PoolWindow(typing.NamedTuple):
    """One candidate cut: its source run, offsets and the cut's dir name."""

    run_dir: pathlib.Path
    from_s: float
    to_s: float
    name: str


def pool_candidates(logs_dir: pathlib.Path, window_s: float = WINDOW_MAX_S) -> list[PoolWindow]:
    """Every complete window of every judged (v3+) run under ``logs_dir``,
    NEWEST cut first (by the cut's own epoch)."""
    out: list[tuple[int, PoolWindow]] = []
    for run_dir in logs_dir.glob("run-*"):
        if not run_dir.is_dir():
            continue
        try:
            epoch_ns = int(run_dir.name[4:])
        except ValueError:
            continue
        version = run_pmlr_version(run_dir)
        if version is None or version < POOL_MIN_PMLR_VERSION:
            continue
        for lo, hi in complete_windows(run_dir, window_s):
            cut_epoch = epoch_ns + int(lo * 1e9)
            out.append((cut_epoch, PoolWindow(run_dir, lo, hi, f"run-{cut_epoch}")))
    out.sort(key=lambda t: t[0], reverse=True)
    return [w for _e, w in out]


def pool_windows(pool_dir: pathlib.Path) -> list[pathlib.Path]:
    """The cuts the pool holds, oldest first (by name = epoch)."""
    if not pool_dir.is_dir():
        return []
    return sorted((p for p in pool_dir.glob("run-*") if p.is_dir()), key=lambda p: p.name)


def pool_ensure(  # noqa: PLR0913 — one parameter per pool knob, deliberately
    logs_dir: pathlib.Path,
    pool_dir: pathlib.Path,
    k: int,
    seed: tuple[pathlib.Path, pathlib.Path] | None,
    *,
    report: typing.Callable[[str], None] | None = None,
    window_s: float = WINDOW_MAX_S,
) -> list[pathlib.Path]:
    """Bring the pool to the NEWEST ``k`` complete windows: reuse cuts that
    exist, cut the missing ones (with their seeds), prune every cut that
    is no longer among the newest ``k`` — pruning by COUNT only. Returns
    the pool's window dirs, oldest first. A reused cut that predates the
    funding seed gets its ``funding-seed.tsv`` back-filled (same law,
    same ``candles.db``) — the pool never needs a re-cut for it."""
    if k <= 0:
        raise WindowError("pool size must be positive")
    wanted = pool_candidates(logs_dir, window_s)[:k]
    wanted_names = {w.name for w in wanted}
    pool_dir.mkdir(parents=True, exist_ok=True)
    existing = {p.name: p for p in pool_windows(pool_dir)}
    for w in wanted:
        if w.name in existing:
            cut = existing[w.name]
            if (
                seed is not None
                and seed[1].is_file()
                and not (cut / claude_worker.seeds.FUNDING_SEED_FILE).is_file()
            ):
                n_desc, n_rows = funding_seed_out(cut, seed[1], int(w.name[4:]) // 1_000_000)
                if report is not None and n_rows:
                    report(
                        f"window-pool: {w.name} {claude_worker.seeds.FUNDING_SEED_FILE}"
                        f" back-filled {n_rows} prints for {n_desc} descriptors"
                    )
            continue
        cut_run(w.run_dir, pool_dir, w.from_s, w.to_s, report=report, seed=seed)
        if report is not None:
            report(f"window-pool: cut {w.name} <- {w.run_dir.name} {w.from_s:.0f}..{w.to_s:.0f} s")
    for name, path in existing.items():
        if name not in wanted_names:
            shutil.rmtree(path, ignore_errors=True)
            if report is not None:
                report(f"window-pool: pruned {name} (beyond the newest {k})")
    return [p for p in pool_windows(pool_dir) if p.name in wanted_names]


def symlink_root(dst: pathlib.Path, windows: typing.Iterable[pathlib.Path]) -> pathlib.Path:
    """A replay root of symlinks to the given window dirs (the bounded
    symlink-root shape) — what a leave-one-window-out run replays."""
    if dst.exists():
        shutil.rmtree(dst)
    dst.mkdir(parents=True)
    for w in windows:
        (dst / w.name).symlink_to(w.resolve(), target_is_directory=True)
    return dst


# ---------------------------------------------------------------
# VRP P2.4 — the CAMPAIGN pool
# ---------------------------------------------------------------
#
# A VRP campaign is nine instants spread over nine hours, and the ≤ 2 h
# capture law forbids replaying the nine hours between them. So the pool
# for this member is not "the newest k complete 2 h windows" — it is the
# NINE slices a campaign actually decides in, cut per UTC day:
#
#   * the decision window `[entry - selection - pad, entry + selection
#     + pad]`, where `entry = expiry - tau`,
#     which holds the strike selection AND the entry;
#   * the first `pad` after every hour boundary strictly between the
#     entry and the expiry — the delta-hedge rebalance instants;
#   * `[expiry - settle_pre, expiry + settle_post]` - the settlement
#     and the unwind.
#
# `backtest --member vrp --replay-dir <pool>` replays them as ONE
# timeline (the harness carries state across runs, and the member's
# clock is wall), so a hedge fires on a tick inside an hour-open slice
# and the settlement on the first record after the expiry, as live.
#
# The minutes BETWEEN the slices are not in the pool, so the member's
# own 24 h warm-up cannot come from the tape — it comes from each
# window's `vrp-seed.tsv`, which `cut_run` already writes from
# `candles.db` as of that window's first instant.

VRP_POOL_DIRNAME: str = "windows-vrp"
#: Deribit settles its daily options at 08:00 UTC.
VRP_EXPIRY_HOUR_UTC: int = 8
#: Slack on each side of the decision window, and the length of each
#: hour-open hedge slice.
CAMPAIGN_PAD_S: float = 300.0
#: `[expiry - this, expiry + CAMPAIGN_SETTLE_POST_S]` - the settlement
#: slice. It opens BEFORE the expiry so the member's last index is the
#: one it settles at (the first record at the expiry instant is a perp
#: tick, not a summary), and closes well after so the perp unwind has
#: ticks to fill against.
CAMPAIGN_SETTLE_PRE_S: float = 600.0
CAMPAIGN_SETTLE_POST_S: float = 1200.0
#: Campaign windows per UTC day: 1 decision + 7 hedge instants + 1
#: settlement, at the artifact's 8 h τ.
CAMPAIGN_WINDOWS_PER_DAY: int = 9
#: Five days of campaigns — the retention the pool is pruned to, by
#: COUNT, exactly as the VM pool is.
CAMPAIGN_POOL_DAYS: int = 5


class CampaignWindow(typing.NamedTuple):
    """One campaign slice as WALL instants, with the name of the instant
    it exists for (`decide`, `hedge-03`, `settle`)."""

    from_ns: int
    to_ns: int
    label: str


def campaign_expiry_ns(day_ms: int) -> int:
    """The daily expiry of the UTC day containing `day_ms`: 08:00Z."""
    day0_ms = (day_ms // 86_400_000) * 86_400_000
    return (day0_ms + VRP_EXPIRY_HOUR_UTC * 3_600_000) * 1_000_000


def campaign_windows(day_ms: int, tau_ns: int, selection_ns: int) -> list[CampaignWindow]:
    """The nine slices of the campaign settling on the UTC day containing
    `day_ms`, in wall order.

    Every slice is far below the 2 h law by construction; the assertion
    is in the test, not here, because a τ the engine refuses never
    reaches this function.
    """
    expiry_ns = campaign_expiry_ns(day_ms)
    entry_ns = expiry_ns - tau_ns
    pad_ns = int(CAMPAIGN_PAD_S * 1e9)
    out: list[CampaignWindow] = [
        CampaignWindow(entry_ns - selection_ns - pad_ns, entry_ns + selection_ns + pad_ns, "decide")
    ]
    # The rebalance instants: every hour boundary strictly inside
    # (entry, expiry). At τ = 8 h and an 08:00Z expiry that is 01:00Z
    # through 07:00Z — seven of them.
    hour_ns = 3_600_000_000_000
    h = (entry_ns // hour_ns + 1) * hour_ns
    while h < expiry_ns:
        out.append(CampaignWindow(h, h + pad_ns, f"hedge-{(h // hour_ns) % 24:02d}"))
        h += hour_ns
    out.append(
        CampaignWindow(
            expiry_ns - int(CAMPAIGN_SETTLE_PRE_S * 1e9),
            expiry_ns + int(CAMPAIGN_SETTLE_POST_S * 1e9),
            "settle",
        )
    )
    return out


def _run_covering(logs_dir: pathlib.Path, wall_ns: int) -> tuple[pathlib.Path, int, int] | None:
    """The run whose captured span contains `wall_ns`, as
    `(run_dir, epoch_ns, end_wall_ns)`; None when no run covers it.

    The run's wall clock is `epoch + (ts - ts_first)`, so its span in
    wall time is `[epoch, epoch + (ts_last - ts_first)]`.
    """
    best: tuple[pathlib.Path, int, int] | None = None
    for run_dir in logs_dir.glob("run-*"):
        if not run_dir.is_dir():
            continue
        try:
            epoch_ns = int(run_dir.name[4:])
        except ValueError:
            continue
        span = run_span(run_dir)
        if span is None:
            continue
        end_ns = epoch_ns + (span[1] - span[0])
        if epoch_ns <= wall_ns <= end_ns and (best is None or epoch_ns > best[1]):
            best = (run_dir, epoch_ns, end_ns)
    return best


def campaign_cut(  # noqa: PLR0913 - one parameter per cut input, deliberately
    logs_dir: pathlib.Path,
    pool_dir: pathlib.Path,
    day_ms: int,
    vrp_path: pathlib.Path,
    seed: tuple[pathlib.Path, pathlib.Path] | None,
    *,
    report: typing.Callable[[str], None] | None = None,
) -> list[pathlib.Path]:
    """Cut one UTC day's campaign into `pool_dir`; returns the cuts made,
    oldest first.

    A slice no run covers is SKIPPED and reported — a campaign the
    capture missed is not an error, it is a day with no evidence. A
    slice that starts inside a run and outlives it is clamped to that
    run: the harness replays the runs in epoch order anyway, so the
    remainder arrives as the next run's own records.
    """
    descriptor, tau_ns = claude_worker.vrp_seed.read_vrp_toml(vrp_path)
    selection_ns = claude_worker.vrp_seed.read_selection_ns(vrp_path)
    del descriptor  # the cut is a function of the clock alone
    pool_dir.mkdir(parents=True, exist_ok=True)
    made: list[pathlib.Path] = []
    for w in campaign_windows(day_ms, tau_ns, selection_ns):
        found = _run_covering(logs_dir, w.from_ns)
        if found is None:
            if report is not None:
                report(f"campaign-pool: {w.label} {w.from_ns} has no run - skipped")
            continue
        run_dir, epoch_ns, end_ns = found
        from_s = (w.from_ns - epoch_ns) / 1e9
        to_s = (min(w.to_ns, end_ns) - epoch_ns) / 1e9
        if to_s <= from_s:
            if report is not None:
                report(f"campaign-pool: {w.label} is empty in {run_dir.name} - skipped")
            continue
        cut = cut_run(run_dir, pool_dir, from_s, to_s, report=report, seed=seed, vrp=vrp_path)
        made.append(cut)
        if report is not None:
            report(
                f"campaign-pool: cut {cut.name} ({w.label}) <- {run_dir.name}"
                f" {from_s:.0f}..{to_s:.0f} s"
            )
    return sorted(made, key=lambda p: p.name)


def campaign_prune(
    pool_dir: pathlib.Path,
    k: int,
    report: typing.Callable[[str], None] | None = None,
) -> list[pathlib.Path]:
    """Keep the newest `k` cuts in the campaign pool, by COUNT — the VM
    pool's law, for the same reason: a byte budget would prune a day's
    campaign in half and leave a settlement with no entry."""
    if k <= 0:
        raise WindowError("pool size must be positive")
    have = pool_windows(pool_dir)
    for path in have[: max(0, len(have) - k)]:
        shutil.rmtree(path, ignore_errors=True)
        if report is not None:
            report(f"campaign-pool: pruned {path.name} (beyond the newest {k})")
    return pool_windows(pool_dir)


def main(argv: list[str] | None = None) -> int:
    """CLI entry point: `--campaign vrp` cuts one UTC day's campaign pool.

    Offline tool, run by hand or by a daily lane — never by the engine.
    """
    ap = argparse.ArgumentParser(prog="claude_worker.window_root")
    ap.add_argument(
        "--campaign",
        required=True,
        choices=("vrp",),
        help="which member's campaign shape to cut",
    )
    ap.add_argument("--logs", required=True, type=pathlib.Path, help="the capture log root")
    ap.add_argument(
        "--pool",
        type=pathlib.Path,
        default=None,
        help=f"the campaign pool (default: <db dir>/{VRP_POOL_DIRNAME})",
    )
    ap.add_argument("--db", required=True, type=pathlib.Path, help="candles.db, for the seeds")
    ap.add_argument("--vrp", required=True, type=pathlib.Path, help="vrp.toml")
    ap.add_argument("--regime", type=pathlib.Path, default=None, help="regime.toml, for the seed")
    ap.add_argument(
        "--day-ms",
        type=int,
        default=None,
        help="any instant in the UTC day whose 08:00Z expiry to cut (default: now)",
    )
    ap.add_argument(
        "--keep-days",
        type=int,
        default=CAMPAIGN_POOL_DAYS,
        help="campaign days to retain (pruned by COUNT)",
    )
    args = ap.parse_args(argv)

    pool = args.pool if args.pool is not None else args.db.parent / VRP_POOL_DIRNAME
    day_ms = args.day_ms if args.day_ms is not None else int(time.time() * 1000)
    seed = (args.regime, args.db) if args.regime is not None else (args.db, args.db)
    made = campaign_cut(
        args.logs, pool, day_ms, args.vrp, seed, report=lambda line: print(line, file=sys.stderr)
    )
    kept = campaign_prune(
        pool,
        args.keep_days * CAMPAIGN_WINDOWS_PER_DAY,
        report=lambda line: print(line, file=sys.stderr),
    )
    print(f"campaign-pool: {len(made)} cut(s) this day, {len(kept)} window(s) in {pool}")
    return 0 if made else 1


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
