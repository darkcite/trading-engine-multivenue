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
  holds and its wall rebase (``wall = epoch + (ts − ts_first)``) lands
  on the true wall clock, so consecutive windows are disjoint runs on
  its virtual timeline;
* the manifests are copied.

Offline tool — allocation is fine; never imported by the engine.
Convention: full ``import x`` only. No ``from x import y``.
"""

import mmap
import os
import pathlib
import shutil
import struct
import typing

HEADER_SIZE: int = 64
_HEADER = struct.Struct("<4sHBxQ")
SLOT_KIND_DEPTH: int = 7
DEPTH_SLOT: int = 192
SLOT: int = 64
WINDOW_MAX_S: float = 2 * 3600.0
MANIFESTS: tuple[str, ...] = ("instrument-manifest.tsv", "options-manifest.tsv")


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


def _cut_file(src: pathlib.Path, dst: pathlib.Path, lo_ts: int, hi_ts: int, epoch_ns: int) -> tuple[int, int]:
    """Copy the slots with ``lo_ts <= ts < hi_ts``; header epoch rewritten."""
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
            with dst.open("wb") as out:
                out.write(bytes(head))
                if b > a:
                    out.write(mm[HEADER_SIZE + a * slot : HEADER_SIZE + b * slot])
        finally:
            mm.close()
    return n, max(b - a, 0)


def cut_run(
    run_dir: pathlib.Path,
    dst_root: pathlib.Path,
    from_s: float,
    to_s: float,
    report: typing.Callable[[str], None] | None = None,
) -> pathlib.Path:
    """Materialise the window ``[from_s, to_s)`` of ``run_dir`` under
    ``dst_root`` as ``run-<epoch + from_s>``; returns the new run dir."""
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
    return out_dir
