# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The archiver: push, verify, list, pull, gc, status (phase S3).

S3 has no directory transaction, so "is this run safely in the bucket?" has to
be answered by a single object rather than inferred from a listing. The commit
order is therefore fixed and total:

    every data object  ->  <run>/_manifest.json  ->  v1/index/<host>/<run>.json

**A run is complete in the bucket if and only if its index object exists**
(S-LAW 5). A crash anywhere earlier leaves objects that no reader consults and
that the next attempt resumes over. That is what makes it safe for
``retention.sh`` to delete a local run only after ``verify`` exits 0 (S-LAW 3):
the question it asks has one authoritative answer, not a heuristic.

Two more rules exist because the alternative loses data:

- **Never upload an open run** (S-LAW 4). Only a run that is not the newest
  under the log root is eligible, because the engine only ever writes into the
  newest one. ``--force`` still refuses while any ``*.pmlr`` was touched in the
  last 60 s or an engine process is alive — a torn tail archived as truth is
  worse than no archive.
- **Budget out, don't get killed.** When the wall budget is spent the push
  stops between files and exits 5 with no manifest and no index written, so
  tomorrow's cycle resumes rather than restarts.

This module allocates freely and is not a hot path — it is offline Python that
reads closed capture directories. It calls ``window_root`` and ``features``; it
never modifies them.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import collections.abc
import compression.zstd
import dataclasses
import fnmatch
import hashlib
import json
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import time
import typing

import httpx

import claude_worker.archive_config
import claude_worker.features
import claude_worker.objstore
import claude_worker.window_root

TOOL_VERSION: typing.Final[str] = "archive/0.1.0"
MANIFEST_VERSION: typing.Final[int] = 1

#: PMLR header: magic, version, slot_kind, pad, epoch_ns (docs/wire-format.md).
_HEADER = struct.Struct("<4sHBxQ")
HEADER_SIZE: typing.Final[int] = 64
_SLOT_192_KIND: typing.Final[int] = 7

CHUNK: typing.Final[int] = 8 * 1024 * 1024
GIB: typing.Final[int] = 1024**3

#: A run whose newest PMLR was touched this recently is treated as live even
#: under --force. Capture flushes every 1 s, so 60 s is 60 flush intervals of
#: slack against a clock skew or a stalled venue.
FRESH_MTIME_S: typing.Final[float] = 60.0

#: Free space an UPLOAD must leave on the volume.
#:
#: DELIBERATELY NOT ``cache_min_free_gib`` (the plan's §7 S3 step 3 said to use
#: it). That number equals retention's ``MIN_FREE_GIB``, and retention only
#: fires BELOW it — so gating uploads on it makes the subsystem
#: self-deadlocking: exactly when retention wants to delete, the archiver
#: refuses to create the verified copy that would let it. On this deployment
#: that is not hypothetical; the volume sits under the floor today, so a push
#: would have exited 4 forever and the backlog could never start.
#:
#: The floor's real job is to stop the archiver filling the disk. An upload
#: stages ONE compressed file at a time — of the order of a hundred megabytes,
#: not a run — so a small absolute reserve protects the volume without blocking
#: the work that frees it. The full ``cache_min_free_gib`` floor still governs
#: PULLS, which do add a whole run to the cache.
STAGING_RESERVE_GIB: typing.Final[float] = 2.0

# Exit codes. One table, relied on by retention.sh and by the cycle script.
EXIT_OK: typing.Final[int] = 0
EXIT_FAILED: typing.Final[int] = 1  # verify failed, or an upload error
EXIT_REFUSED: typing.Final[int] = 2  # newest run without --force, or guard hit
EXIT_DISABLED: typing.Final[int] = 3  # subsystem off on a GATING lane
EXIT_NO_SPACE: typing.Final[int] = 4  # free-space floor would be breached
EXIT_BUDGET: typing.Final[int] = 5  # budget spent mid-run — resumable


class ArchiveError(Exception):
    """A lane failed in a way that maps onto an exit code."""

    def __init__(self, message: str, code: int = EXIT_FAILED) -> None:
        super().__init__(message)
        self.code = code


# --------------------------------------------------------------------------
# PMLR facts, read straight off the file
# --------------------------------------------------------------------------


def _slot_size(kind: int) -> int:
    return 192 if kind == _SLOT_192_KIND else 64


def pmlr_facts(path: pathlib.Path) -> dict[str, typing.Any]:
    """Version, slot kind, slot count and ts span for a PMLR file.

    Everything is null for a non-PMLR file or a header-only one (exactly 64 B —
    never 0 B; 20 of a run's ~37 files are normally header-only).
    """
    blank: dict[str, typing.Any] = {
        "pmlr_version": None,
        "slot_kind": None,
        "slots": None,
        "first_ts_ns": None,
        "last_ts_ns": None,
    }
    if path.suffix != ".pmlr":
        return blank
    size = path.stat().st_size
    if size < HEADER_SIZE:
        return blank
    with path.open("rb") as handle:
        head = handle.read(HEADER_SIZE)
    magic, version, kind, _epoch_ns = _HEADER.unpack(head[: _HEADER.size])
    if magic != b"PMLR":
        return blank
    slot = _slot_size(kind)
    slots = (size - HEADER_SIZE) // slot
    span = claude_worker.window_root.first_last_ts(path) if slots else None
    return {
        "pmlr_version": version,
        "slot_kind": kind,
        "slots": slots,
        "first_ts_ns": span[0] if span else None,
        "last_ts_ns": span[1] if span else None,
    }


def _digests(path: pathlib.Path) -> tuple[str, str, int]:
    sha = hashlib.sha256()
    md5 = hashlib.md5()
    size = 0
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(CHUNK)
            if not chunk:
                break
            sha.update(chunk)
            md5.update(chunk)
            size += len(chunk)
    return sha.hexdigest(), md5.hexdigest(), size


def compress_to(src: pathlib.Path, dst: pathlib.Path, level: int) -> tuple[str, str, int]:
    """zstd ``src`` into ``dst``; return (sha256, md5, size) of the STORED bytes.

    The compressed stream has to exist on disk before the request is signed:
    SigV4 needs the payload digest and the exact length up front, and this
    client never uses UNSIGNED-PAYLOAD (the server-side digest check is a free
    integrity guarantee on every upload).
    """
    dst.parent.mkdir(parents=True, exist_ok=True)
    compressor = compression.zstd.ZstdCompressor(level=level)
    sha = hashlib.sha256()
    md5 = hashlib.md5()
    size = 0
    with src.open("rb") as reader, dst.open("wb") as writer:
        while True:
            chunk = reader.read(CHUNK)
            if not chunk:
                break
            blob = compressor.compress(chunk)
            if blob:
                writer.write(blob)
                sha.update(blob)
                md5.update(blob)
                size += len(blob)
        blob = compressor.flush()
        if blob:
            writer.write(blob)
            sha.update(blob)
            md5.update(blob)
            size += len(blob)
    return sha.hexdigest(), md5.hexdigest(), size


def decompress_to(src: pathlib.Path, dst: pathlib.Path) -> str:
    """Inflate a stored object; return the sha256 of the ORIGINAL bytes."""
    dst.parent.mkdir(parents=True, exist_ok=True)
    decompressor = compression.zstd.ZstdDecompressor()
    sha = hashlib.sha256()
    with src.open("rb") as reader, dst.open("wb") as writer:
        while True:
            chunk = reader.read(CHUNK)
            if not chunk:
                break
            blob = decompressor.decompress(chunk)
            if blob:
                writer.write(blob)
                sha.update(blob)
    return sha.hexdigest()


# --------------------------------------------------------------------------
# eligibility
# --------------------------------------------------------------------------


def is_newest_run(run_dir: pathlib.Path) -> bool:
    runs = claude_worker.features.run_dirs(run_dir.parent)
    return bool(runs) and runs[-1].name == run_dir.name


def engine_is_running() -> bool:
    try:
        done = subprocess.run(
            ["pgrep", "-f", "multivenue-engine ru[n]"], capture_output=True, check=False
        )
    except OSError:
        return False
    return done.returncode == 0


def newest_pmlr_age_s(run_dir: pathlib.Path, now: float | None = None) -> float:
    stamps = [p.stat().st_mtime for p in run_dir.glob("*.pmlr")]
    if not stamps:
        return float("inf")
    return (time.time() if now is None else now) - max(stamps)


def assert_eligible(run_dir: pathlib.Path, *, force: bool, now: float | None = None) -> None:
    """S-LAW 4. A closed run is closed by construction; --force must earn it."""
    if not is_newest_run(run_dir):
        return
    if not force:
        raise ArchiveError(
            f"{run_dir.name} is the newest run — the engine writes there (use --force)",
            EXIT_REFUSED,
        )
    age = newest_pmlr_age_s(run_dir, now)
    if age < FRESH_MTIME_S:
        raise ArchiveError(
            f"{run_dir.name}: a capture file changed {age:.0f}s ago — refusing --force",
            EXIT_REFUSED,
        )
    if engine_is_running():
        raise ArchiveError(
            f"{run_dir.name}: an engine process is alive — refusing --force", EXIT_REFUSED
        )


def eligible_files(
    run_dir: pathlib.Path, skip_globs: collections.abc.Sequence[str]
) -> list[pathlib.Path]:
    out: list[pathlib.Path] = []
    for path in sorted(p for p in run_dir.iterdir() if p.is_file()):
        if any(fnmatch.fnmatch(path.name, pattern) for pattern in skip_globs):
            continue
        out.append(path)
    return out


def free_gib(path: pathlib.Path) -> float:
    probe = path
    while not probe.exists() and probe != probe.parent:
        probe = probe.parent
    return shutil.disk_usage(probe).free / GIB


# --------------------------------------------------------------------------
# progress (resume)
# --------------------------------------------------------------------------


def _read_progress(path: pathlib.Path) -> dict[str, typing.Any]:
    try:
        loaded = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return loaded if isinstance(loaded, dict) else {}


def _write_progress(path: pathlib.Path, data: dict[str, typing.Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=1, sort_keys=True), encoding="utf-8")
    os.replace(tmp, path)


# --------------------------------------------------------------------------
# the catalog (best effort, never a failure)
# --------------------------------------------------------------------------


def capture_catalog(run_dir: pathlib.Path) -> tuple[typing.Any, str]:
    """`multivenue-engine capture-catalog --dir <run>`; JSON is always stdout.

    A missing or failing binary yields (None, reason). The catalog is what lets
    the agent answer coverage questions without pulling a byte of PMLR, but it
    is never worth failing an upload over.
    """
    try:
        done = subprocess.run(
            ["multivenue-engine", "capture-catalog", "--dir", str(run_dir)],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return None, "absent"
    if done.returncode != 0:
        return None, f"exit {done.returncode}"
    try:
        return json.loads(done.stdout), ""
    except ValueError:
        return None, "unparseable stdout"


# --------------------------------------------------------------------------
# manifest
# --------------------------------------------------------------------------


def build_manifest(  # noqa: PLR0913 — one parameter per manifest input, deliberately
    cfg: claude_worker.archive_config.ArchiveConfig,
    run_dir: pathlib.Path,
    files: list[dict[str, typing.Any]],
    *,
    catalog: typing.Any,
    catalog_error: str,
    now_ns: int,
) -> dict[str, typing.Any]:
    span = claude_worker.window_root.run_span(run_dir)
    complete = claude_worker.window_root.complete_windows(run_dir)
    manifest: dict[str, typing.Any] = {
        "archive_manifest_version": MANIFEST_VERSION,
        "run": run_dir.name,
        "epoch_ns": int(run_dir.name[len("run-") :]),
        "host_id": cfg.host_id,
        "uploaded_at_ns": now_ns,
        "tool_version": TOOL_VERSION,
        "stored_encoding": "zstd",
        "zstd_level": cfg.zstd_level,
        "files": files,
        "totals": {
            "files": len(files),
            "size_bytes": sum(int(f["size_bytes"]) for f in files),
            "stored_bytes": sum(int(f["stored_bytes"]) for f in files),
        },
        "span_ns": list(span) if span else None,
        "pmlr_version": claude_worker.window_root.run_pmlr_version(run_dir),
        # complete_windows, NOT windows_of: the pool law counts only FULL
        # slices, and windows_of includes the partial tail.
        "windows_2h_complete": len(complete),
        "catalog": catalog,
        "catalog_note": (
            "venue_totals is authoritative for per-venue ticks; the per-day "
            "venue_ticks array omits bybit (capture_catalog.rs)"
        ),
    }
    if catalog_error:
        manifest["catalog_error"] = catalog_error
    return manifest


def manifest_bytes(manifest: dict[str, typing.Any]) -> bytes:
    """One canonical encoding. The index object is these SAME bytes."""
    return json.dumps(manifest, indent=1, sort_keys=True).encode("utf-8")


# --------------------------------------------------------------------------
# the archiver
# --------------------------------------------------------------------------


@dataclasses.dataclass(slots=True)
class PushResult:
    run: str
    files: int
    size_bytes: int
    stored_bytes: int
    parts: int
    elapsed_s: float
    skipped: bool = False

    def tell(self) -> str:
        if self.skipped:
            return f"archive: run={self.run} already complete"
        ratio = (self.size_bytes / self.stored_bytes) if self.stored_bytes else 0.0
        return (
            f"archive: run={self.run} files={self.files} "
            f"bytes={_human(self.size_bytes)} stored={_human(self.stored_bytes)} "
            f"ratio={ratio:.1f}x elapsed={self.elapsed_s:.0f}s parts={self.parts}"
        )


_SCALE: typing.Final[float] = 1024.0


def _human(value: int) -> str:
    step = float(value)
    for unit in ("B", "K", "M", "G", "T"):
        if step < _SCALE or unit == "T":
            return f"{step:.0f}{unit}" if unit == "B" else f"{step:.2f}{unit}"
        step /= _SCALE
    return f"{step:.2f}T"


class Archiver:
    def __init__(
        self,
        cfg: claude_worker.archive_config.ArchiveConfig,
        store: claude_worker.objstore.ObjectStore,
        *,
        clock: collections.abc.Callable[[], float] = time.monotonic,
        now_ns: collections.abc.Callable[[], int] = time.time_ns,
    ) -> None:
        self.cfg = cfg
        self.store = store
        self._clock = clock
        self._now_ns = now_ns

    # -- push ------------------------------------------------------------

    def push_run(  # noqa: PLR0912, PLR0915 — the commit order is one flat sequence
        self,
        run_dir: pathlib.Path,
        *,
        force: bool = False,
        dry_run: bool = False,
        budget: claude_worker.objstore.Budget | None = None,
        with_catalog: bool = True,
    ) -> PushResult:
        run_dir = run_dir.resolve()
        run_id = run_dir.name
        if not run_id.startswith("run-") or not run_id[len("run-") :].isdigit():
            raise ArchiveError(f"{run_id} is not a run-<epoch_ns> directory", EXIT_REFUSED)
        started = self._clock()

        # Already complete? The index object is the only truth (S-LAW 5).
        if self.store.head(self.cfg.index_key(run_id)) is not None:
            return PushResult(run_id, 0, 0, 0, 0, self._clock() - started, skipped=True)

        assert_eligible(run_dir, force=force)

        # A previous attempt died between the manifest and the index: finish it
        # rather than re-uploading a gigabyte.
        existing = self.store.head(self.cfg.manifest_key(run_id))
        if existing is not None:
            raw = self.store.get_bytes(self.cfg.manifest_key(run_id))
            self._verify_manifest(json.loads(raw))
            self.store.put_bytes(self.cfg.index_key(run_id), raw)
            return PushResult(run_id, 0, 0, 0, 0, self._clock() - started, skipped=True)

        files = eligible_files(run_dir, self.cfg.skip_globs)
        if not files:
            raise ArchiveError(f"{run_id}: no files to push", EXIT_REFUSED)

        staging = self.cfg.staging_dir(run_id)
        largest = max(p.stat().st_size for p in files)
        # A conservative compressed-size bound: zstd on PMLR measured ~4.9x on
        # ticks, so a quarter of the original is pessimistic on purpose.
        staged_bound_gib = largest / 4 / GIB
        if free_gib(self.cfg.cache_dir) - staged_bound_gib < STAGING_RESERVE_GIB:
            raise ArchiveError(
                f"{run_id}: staging would leave under {STAGING_RESERVE_GIB} GiB free",
                EXIT_NO_SPACE,
            )

        if dry_run:
            total = sum(p.stat().st_size for p in files)
            return PushResult(run_id, len(files), total, 0, 0, self._clock() - started)

        progress_path = staging / "progress.json"
        progress = _read_progress(progress_path)
        records: dict[str, typing.Any] = dict(progress.get("files", {}))
        parts_total = 0
        done = 0

        for path in files:
            key = self.cfg.run_key(run_id, path.name)
            recorded = records.get(path.name)
            if recorded is not None:
                head = self.store.head(key)
                if head is not None and head.size == int(recorded["stored_bytes"]):
                    done += 1
                    parts_total += int(recorded.get("parts", 1))
                    continue

            sha256_hex, _md5, size = _digests(path)
            staged = staging / (path.name + ".zst")
            stored_sha, stored_md5, stored_size = compress_to(
                path, staged, self.cfg.zstd_level
            )
            try:
                head = self.store.head(key)
                if head is not None and head.size != stored_size:
                    # A torn previous attempt. This is the ONLY place an
                    # overwrite is permitted (S-LAW 11's narrow clause).
                    head = None
                if head is None:
                    if stored_size >= self.cfg.part_size:
                        meta = self.store.multipart_put(
                            key, staged, part_size=self.cfg.part_size, budget=budget
                        )
                        parts = (stored_size + self.cfg.part_size - 1) // self.cfg.part_size
                    else:
                        meta = self.store.put(
                            key, staged, sha256_hex=stored_sha, md5_hex=stored_md5
                        )
                        parts = 1
                    etag = meta.etag
                else:
                    etag = head.etag
                    parts = int(recorded.get("parts", 1)) if recorded else 1
            finally:
                staged.unlink(missing_ok=True)

            facts = pmlr_facts(path)
            records[path.name] = {
                "name": path.name,
                "key": key,
                "size_bytes": size,
                "sha256": sha256_hex,
                "stored_bytes": stored_size,
                "stored_sha256": stored_sha,
                "etag": etag,
                "parts": parts,
                **facts,
            }
            parts_total += parts
            done += 1
            _write_progress(progress_path, {"run": run_id, "files": records})

            if budget is not None and budget.spent() and done < len(files):
                raise ArchiveError(
                    f"{run_id}: budget spent after {done}/{len(files)} files", EXIT_BUDGET
                )

        catalog, catalog_error = (
            capture_catalog(run_dir) if with_catalog else (None, "skipped")
        )
        ordered = [records[p.name] for p in files]
        manifest = build_manifest(
            self.cfg,
            run_dir,
            ordered,
            catalog=catalog,
            catalog_error=catalog_error,
            now_ns=self._now_ns(),
        )
        payload = manifest_bytes(manifest)

        # Manifest, then read it back, then the index with the SAME bytes.
        self.store.put_bytes(self.cfg.manifest_key(run_id), payload)
        if self.store.get_bytes(self.cfg.manifest_key(run_id)) != payload:
            raise ArchiveError(f"{run_id}: manifest did not read back identically", EXIT_FAILED)
        self.store.put_bytes(self.cfg.index_key(run_id), payload)

        shutil.rmtree(staging, ignore_errors=True)
        totals = manifest["totals"]
        return PushResult(
            run_id,
            int(totals["files"]),
            int(totals["size_bytes"]),
            int(totals["stored_bytes"]),
            parts_total,
            self._clock() - started,
        )

    def push_pending(
        self,
        root: pathlib.Path,
        *,
        budget: claude_worker.objstore.Budget | None = None,
        dry_run: bool = False,
        on_result: collections.abc.Callable[[PushResult], None] | None = None,
    ) -> list[PushResult]:
        """Stage A: every closed run not yet complete, oldest first.

        ``on_result`` is called as each run lands rather than at the end. A
        backfill is hours of work; without it the operator watches a silent log
        and cannot tell progress from a hang.
        """
        runs = claude_worker.features.run_dirs(root)
        out: list[PushResult] = []
        for run_dir in runs[:-1]:  # never the newest (S-LAW 4)
            if budget is not None and budget.spent():
                raise ArchiveError("budget spent between runs", EXIT_BUDGET)
            result = self.push_run(run_dir, budget=budget, dry_run=dry_run)
            out.append(result)
            if on_result is not None:
                on_result(result)
        return out

    # -- verify ----------------------------------------------------------

    def _verify_manifest(self, manifest: dict[str, typing.Any]) -> None:
        for entry in manifest.get("files", []):
            head = self.store.head(entry["key"])
            if head is None:
                raise ArchiveError(f"{entry['key']}: missing from the bucket", EXIT_FAILED)
            if head.size != int(entry["stored_bytes"]):
                raise ArchiveError(
                    f"{entry['key']}: size {head.size} != manifest {entry['stored_bytes']}",
                    EXIT_FAILED,
                )
            if entry.get("etag") and head.etag and head.etag != entry["etag"]:
                raise ArchiveError(f"{entry['key']}: ETag mismatch", EXIT_FAILED)

    def verify(self, run_id: str, *, deep: bool = False) -> dict[str, typing.Any]:
        """The question retention.sh asks before it deletes anything."""
        index = self.store.head(self.cfg.index_key(run_id))
        if index is None:
            raise ArchiveError(f"{run_id}: no index object — not complete", EXIT_FAILED)
        index_bytes = self.store.get_bytes(self.cfg.index_key(run_id))
        manifest_raw = self.store.get_bytes(self.cfg.manifest_key(run_id))
        if index_bytes != manifest_raw:
            raise ArchiveError(f"{run_id}: index and manifest bytes differ", EXIT_FAILED)
        manifest = json.loads(manifest_raw)
        self._verify_manifest(manifest)
        if deep:
            self._verify_deep(manifest)
        return typing.cast(dict[str, typing.Any], manifest)

    def _verify_deep(self, manifest: dict[str, typing.Any]) -> None:
        staging = self.cfg.cache_dir / "verify"
        staging.mkdir(parents=True, exist_ok=True)
        try:
            for entry in manifest["files"]:
                tmp = staging / (entry["name"] + ".zst")
                self.store.get(entry["key"], tmp, expect_size=int(entry["stored_bytes"]))
                digest = hashlib.sha256(tmp.read_bytes()).hexdigest()
                tmp.unlink(missing_ok=True)
                if digest != entry["stored_sha256"]:
                    raise ArchiveError(f"{entry['key']}: stored sha256 mismatch", EXIT_FAILED)
        finally:
            shutil.rmtree(staging, ignore_errors=True)

    # -- read lanes ------------------------------------------------------

    def list_runs(self, since_ns: int | None = None) -> list[dict[str, typing.Any]]:
        start_after = f"{self.cfg.index_prefix()}run-{since_ns}" if since_ns else None
        out: list[dict[str, typing.Any]] = []
        cache = self.cfg.manifest_cache_dir()
        cache.mkdir(parents=True, exist_ok=True)
        for meta in self.store.list(self.cfg.index_prefix(), start_after=start_after):
            run_id = meta.key.rsplit("/", 1)[-1].removesuffix(".json")
            cached = cache / f"{run_id}.json"
            if cached.is_file() and cached.stat().st_size == meta.size:
                raw = cached.read_bytes()
            else:
                raw = self.store.get_bytes(meta.key)
                cached.write_bytes(raw)
            out.append(json.loads(raw))
        return out

    # -- S7: legacy tarballs and derived stores --------------------------

    def push_tarball(self, path: pathlib.Path) -> PushResult:
        """A pre-existing ``run-<ns>.tar.gz`` from the old compress lane.

        Stored verbatim: the tarball IS the artefact, and re-packing it would
        destroy the only evidence that it is what retention actually wrote. The
        index-last law applies unchanged, so a half-uploaded tarball is
        invisible exactly like a half-uploaded run.

        Local tarballs are never deleted here — pruning ~/multivenue/archive
        stays a deliberate operator act, as it always was.
        """
        started = self._clock()
        run_id = path.name.removesuffix(".tar.gz")
        index_key = f"{self.cfg.prefix}/tarballs/{self.cfg.host_id}/{run_id}.json"
        object_key = f"{self.cfg.tarballs_prefix()}{run_id}.tar.gz"
        if self.store.head(index_key) is not None:
            return PushResult(run_id, 0, 0, 0, 0, self._clock() - started, skipped=True)

        sha256_hex, md5_hex, size = _digests(path)
        head = self.store.head(object_key)
        if head is None or head.size != size:
            if size >= self.cfg.part_size:
                self.store.multipart_put(object_key, path, part_size=self.cfg.part_size)
                parts = (size + self.cfg.part_size - 1) // self.cfg.part_size
            else:
                self.store.put(object_key, path, sha256_hex=sha256_hex, md5_hex=md5_hex)
                parts = 1
        else:
            parts = 1

        manifest = {
            "archive_manifest_version": MANIFEST_VERSION,
            "kind": "tarball",
            "run": run_id,
            "host_id": self.cfg.host_id,
            "key": object_key,
            "size_bytes": size,
            "sha256": sha256_hex,
            "uploaded_at_ns": self._now_ns(),
            "tool_version": TOOL_VERSION,
        }
        self.store.put_bytes(index_key, manifest_bytes(manifest))
        return PushResult(run_id, 1, size, size, parts, self._clock() - started)

    def tarball_index_key(self, run_id: str) -> str:
        return f"{self.cfg.prefix}/tarballs/{self.cfg.host_id}/{run_id}.json"

    def list_tarballs(self) -> dict[str, dict[str, typing.Any]]:
        """Every run held as a legacy tarball rather than as a directory."""
        out: dict[str, dict[str, typing.Any]] = {}
        prefix = f"{self.cfg.prefix}/tarballs/{self.cfg.host_id}/"
        for meta in self.store.list(prefix):
            if not meta.key.endswith(".json"):
                continue
            manifest = json.loads(self.store.get_bytes(meta.key))
            out[manifest["run"]] = manifest
        return out

    def verify_tarball(self, run_id: str) -> dict[str, typing.Any]:
        index = self.store.head(self.tarball_index_key(run_id))
        if index is None:
            raise ArchiveError(f"{run_id}: no tarball index object", EXIT_FAILED)
        manifest = json.loads(self.store.get_bytes(self.tarball_index_key(run_id)))
        head = self.store.head(manifest["key"])
        if head is None:
            raise ArchiveError(f"{manifest['key']}: missing from the bucket", EXIT_FAILED)
        if head.size != int(manifest["size_bytes"]):
            raise ArchiveError(f"{manifest['key']}: size disagrees with the manifest", EXIT_FAILED)
        return typing.cast(dict[str, typing.Any], manifest)

    def pull_tarball(self, run_id: str, into: pathlib.Path) -> pathlib.Path:
        """Materialise a run that was archived as a legacy ``.tar.gz``.

        Same invisibility law as a directory pull: everything happens under a
        ``.partial`` suffix, which breaks the ``run-<digits>`` name that every
        discovery law keys on, so an interrupted extraction is nothing rather
        than a half-run some backtest quietly reads.

        The tarball's sha256 is checked BEFORE extraction — untarring an
        unverified archive would scatter unverified files across the cache.
        """
        manifest = self.verify_tarball(run_id)
        partial = into / f"{run_id}.partial"
        shutil.rmtree(partial, ignore_errors=True)
        partial.mkdir(parents=True, exist_ok=True)
        blob = partial / f"{run_id}.tar.gz"
        try:
            self.store.get(manifest["key"], blob, expect_size=int(manifest["size_bytes"]))
            digest, _md5, _size = _digests(blob)
            if digest != manifest["sha256"]:
                raise ArchiveError(f"{run_id}: tarball sha256 mismatch", EXIT_FAILED)
            done = subprocess.run(
                ["tar", "-xzf", str(blob), "-C", str(partial)],
                capture_output=True,
                check=False,
            )
            if done.returncode != 0:
                raise ArchiveError(f"{run_id}: tar -xzf exited {done.returncode}", EXIT_FAILED)
            blob.unlink(missing_ok=True)
            # retention tarred with `-C <log root> <name>`, so the archive holds
            # a single `run-<ns>/` directory. Tolerate a flat layout too.
            inner = partial / run_id
            source = inner if inner.is_dir() else partial
            final = into / run_id
            shutil.rmtree(final, ignore_errors=True)
            if source is inner:
                os.replace(inner, final)
                shutil.rmtree(partial, ignore_errors=True)
            else:
                os.replace(partial, final)
        except BaseException:
            shutil.rmtree(partial, ignore_errors=True)
            raise
        marker = into / f".{run_id}.complete"
        marker.write_text(
            json.dumps({"manifest_sha256": hashlib.sha256(manifest_bytes(manifest)).hexdigest()}),
            encoding="utf-8",
        )
        return final

    def push_derived(self, kind: str, path: pathlib.Path) -> PushResult:
        """A derived store (candles db, a report, a features day).

        Derived data is reproducible in principle and precious in practice —
        it is what the gates were computed against. Same index-last law.
        """
        started = self._clock()
        key = f"{self.cfg.derived_prefix(kind)}{path.name}"
        sha256_hex, md5_hex, size = _digests(path)
        head = self.store.head(key)
        if head is not None and head.size == size:
            return PushResult(path.name, 0, 0, 0, 0, self._clock() - started, skipped=True)
        if size >= self.cfg.part_size:
            self.store.multipart_put(key, path, part_size=self.cfg.part_size)
            parts = (size + self.cfg.part_size - 1) // self.cfg.part_size
        else:
            self.store.put(key, path, sha256_hex=sha256_hex, md5_hex=md5_hex)
            parts = 1
        return PushResult(path.name, 1, size, size, parts, self._clock() - started)

    def pull(self, run_id: str, into: pathlib.Path) -> pathlib.Path:
        """Materialise a run. Verifies every file's ORIGINAL sha256.

        The destination is assembled under a ``.partial`` suffix, which breaks
        the ``run-<digits>`` name every discovery law keys on — an interrupted
        pull is therefore invisible to `discover_runs`, `run_dirs` and
        `select_runs` rather than being served as a truncated run.
        """
        manifest = self.verify(run_id)
        partial = into / f"{run_id}.partial"
        shutil.rmtree(partial, ignore_errors=True)
        partial.mkdir(parents=True, exist_ok=True)
        staging = partial / ".staged"
        staging.mkdir(exist_ok=True)
        for entry in manifest["files"]:
            stored = staging / (entry["name"] + ".zst")
            self.store.get(entry["key"], stored, expect_size=int(entry["stored_bytes"]))
            digest = decompress_to(stored, partial / entry["name"])
            stored.unlink(missing_ok=True)
            if digest != entry["sha256"]:
                raise ArchiveError(
                    f"{entry['name']}: sha256 mismatch after decompression", EXIT_FAILED
                )
        shutil.rmtree(staging, ignore_errors=True)
        final = into / run_id
        shutil.rmtree(final, ignore_errors=True)
        os.replace(partial, final)
        marker = final.parent / f".{run_id}.complete"
        marker.write_text(
            json.dumps({"manifest_sha256": hashlib.sha256(manifest_bytes(manifest)).hexdigest()}),
            encoding="utf-8",
        )
        return final


# --------------------------------------------------------------------------
# construction + CLI
# --------------------------------------------------------------------------


def build(
    cfg: claude_worker.archive_config.ArchiveConfig,
    client: httpx.Client | None = None,
) -> Archiver:
    if not claude_worker.archive_config.is_enabled(cfg):
        raise ArchiveError(f"archive: disabled ({cfg.reason})", EXIT_DISABLED)
    assert cfg.endpoint is not None and cfg.creds is not None
    store = claude_worker.objstore.ObjectStore(
        cfg.endpoint,
        cfg.creds,
        client if client is not None else httpx.Client(),
        timeout_s=cfg.timeout_s,
    )
    return Archiver(cfg, store)


def _replay_dir(explicit: str) -> pathlib.Path:
    if explicit:
        return pathlib.Path(explicit).expanduser()
    return pathlib.Path(
        os.environ.get("CLAUDE_WORKER_REPLAY_DIR", "") or "~/multivenue/logs"
    ).expanduser()


def _budget(seconds: float) -> claude_worker.objstore.Budget | None:
    if seconds <= 0:
        return None
    return claude_worker.objstore.Budget(deadline=time.monotonic() + seconds)


def gc_cache(cfg: claude_worker.archive_config.ArchiveConfig, max_gib: float) -> dict[str, int]:
    """Local cache only (S-LAW 9). Never touches the bucket."""
    cache = cfg.cache_dir
    removed = {"partial": 0, "runs": 0}
    if not cache.is_dir():
        return removed
    for child in sorted(cache.glob("*.partial")):
        shutil.rmtree(child, ignore_errors=True)
        removed["partial"] += 1
    runs = sorted(
        (p for p in cache.glob("run-*") if p.is_dir()),
        key=lambda p: (cache / f".{p.name}.complete").stat().st_mtime
        if (cache / f".{p.name}.complete").exists()
        else 0.0,
    )
    limit = max_gib * GIB
    total = sum(
        sum(f.stat().st_size for f in run.rglob("*") if f.is_file()) for run in runs
    )
    for run in runs:
        if total <= limit:
            break
        size = sum(f.stat().st_size for f in run.rglob("*") if f.is_file())
        shutil.rmtree(run, ignore_errors=True)
        (cache / f".{run.name}.complete").unlink(missing_ok=True)
        total -= size
        removed["runs"] += 1
    return removed


def main(argv: collections.abc.Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="multivenue-archive", description="object-storage archive for capture runs"
    )
    sub = parser.add_subparsers(dest="verb", required=True)

    push = sub.add_parser("push-run")
    push.add_argument("run_dir")
    push.add_argument("--force", action="store_true")
    push.add_argument("--dry-run", action="store_true")
    push.add_argument("--budget-s", type=float, default=0.0)
    push.add_argument("--no-catalog", action="store_true")

    pending = sub.add_parser("push-pending")
    pending.add_argument("--root", default="")
    pending.add_argument("--budget-s", type=float, default=0.0)
    pending.add_argument("--dry-run", action="store_true")

    verify = sub.add_parser("verify")
    verify.add_argument("run")
    verify.add_argument("--deep", action="store_true")

    listing = sub.add_parser("list")
    listing.add_argument("--since", default="")
    listing.add_argument("--json", action="store_true")

    pull = sub.add_parser("pull")
    pull.add_argument("run")
    pull.add_argument("--into", default="")

    collect = sub.add_parser("gc")
    # -1 means "use the configured cap". NOT 0: `--max-gib 0` is a legitimate
    # request meaning "empty the cache", and `args.max_gib or cfg.cache_max_gib`
    # would silently swallow it as falsy.
    collect.add_argument("--max-gib", type=float, default=-1.0)

    sub.add_parser("status")

    tarball = sub.add_parser("push-tarball")
    tarball.add_argument("path", nargs="?", default="")
    tarball.add_argument("--all", action="store_true")
    tarball.add_argument("--dir", default="~/multivenue/archive")
    tarball.add_argument("--budget-s", type=float, default=0.0)

    derived = sub.add_parser("push-derived")
    derived.add_argument("kind", choices=["candles", "reports", "features"])
    derived.add_argument("--path", default="")
    derived.add_argument("--budget-s", type=float, default=0.0)

    args = parser.parse_args(argv)
    cfg = claude_worker.archive_config.load()

    # Non-gating lanes stay useful with the subsystem off: the cache is local.
    if args.verb in ("status", "gc") and not claude_worker.archive_config.is_enabled(cfg):
        if args.verb == "gc":
            print(json.dumps(gc_cache(cfg, _gc_limit(args, cfg))))
        print(cfg.tell(), file=sys.stderr)
        return EXIT_OK

    try:
        archiver = build(cfg)
        return _dispatch(args, cfg, archiver)
    except ArchiveError as exc:
        print(str(exc), file=sys.stderr)
        return exc.code
    except claude_worker.objstore.ObjStoreError as exc:
        print(f"archive: {exc}", file=sys.stderr)
        return EXIT_FAILED


def _dispatch(  # noqa: PLR0911, PLR0912 — one branch per verb, deliberately flat
    args: argparse.Namespace,
    cfg: claude_worker.archive_config.ArchiveConfig,
    archiver: Archiver,
) -> int:
    if args.verb == "push-run":
        result = archiver.push_run(
            pathlib.Path(args.run_dir).expanduser(),
            force=args.force,
            dry_run=args.dry_run,
            budget=_budget(args.budget_s),
            with_catalog=not args.no_catalog,
        )
        print(result.tell(), file=sys.stderr)
        return EXIT_OK
    if args.verb == "push-pending":

        def report(result: PushResult) -> None:
            print(result.tell(), file=sys.stderr, flush=True)

        archiver.push_pending(
            _replay_dir(args.root),
            budget=_budget(args.budget_s),
            dry_run=args.dry_run,
            on_result=report,
        )
        return EXIT_OK
    if args.verb == "verify":
        run_id = _run_id(args.run)
        if archiver.store.head(cfg.index_key(run_id)) is not None:
            archiver.verify(run_id, deep=args.deep)
        else:
            # retention asks this question about runs it is about to delete, and
            # the oldest week is held as legacy tarballs — answering "no" for
            # those would keep data forever that IS safely in the bucket.
            archiver.verify_tarball(run_id)
        print(f"archive: {run_id} verified", file=sys.stderr)
        return EXIT_OK
    if args.verb == "list":
        since = int(args.since) if args.since.isdigit() else None
        for manifest in archiver.list_runs(since_ns=since):
            print(json.dumps(_list_row(manifest), sort_keys=True))
        return EXIT_OK
    if args.verb == "pull":
        into = pathlib.Path(args.into).expanduser() if args.into else cfg.cache_dir
        run_id = _run_id(args.run)
        if archiver.store.head(cfg.index_key(run_id)) is not None:
            print(str(archiver.pull(run_id, into)))
        else:
            print(str(archiver.pull_tarball(run_id, into)))
        return EXIT_OK
    if args.verb == "gc":
        print(json.dumps(gc_cache(cfg, _gc_limit(args, cfg))))
        return EXIT_OK
    if args.verb == "status":
        runs = archiver.list_runs()
        print(f"{cfg.tell()} runs_remote={len(runs)}", file=sys.stderr)
        return EXIT_OK
    if args.verb == "push-tarball":
        budget = _budget(args.budget_s)
        paths = (
            sorted(pathlib.Path(args.dir).expanduser().glob("run-*.tar.gz"))
            if args.all
            else [pathlib.Path(args.path).expanduser()]
        )
        for path in paths:
            if budget is not None and budget.spent():
                print("archive: budget spent between tarballs", file=sys.stderr)
                return EXIT_BUDGET
            print(archiver.push_tarball(path).tell(), file=sys.stderr)
        return EXIT_OK
    if args.verb == "push-derived":
        budget = _budget(args.budget_s)
        for path in _derived_paths(args.kind, args.path):
            if budget is not None and budget.spent():
                print("archive: budget spent between files", file=sys.stderr)
                return EXIT_BUDGET
            print(archiver.push_derived(args.kind, path).tell(), file=sys.stderr)
        return EXIT_OK
    return EXIT_OK


def _derived_paths(kind: str, explicit: str) -> list[pathlib.Path]:
    if explicit:
        return [pathlib.Path(explicit).expanduser()]
    home = pathlib.Path.home() / "multivenue"
    if kind == "reports":
        return sorted((home / "worker" / "reports").glob("pnl-*.json"))
    if kind == "candles":
        # A live sqlite file is not a file you copy. `.backup` is the only
        # snapshot that is guaranteed internally consistent.
        source = home / "worker" / "candles.db"
        if not source.is_file():
            return []
        stamp = time.strftime("%Y%m%d", time.gmtime())
        snapshot = home / "s3-cache" / "staging" / f"candles-{stamp}.db"
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        done = subprocess.run(
            ["sqlite3", str(source), f".backup '{snapshot}'"], capture_output=True, check=False
        )
        if done.returncode != 0 or not snapshot.is_file():
            raise ArchiveError("candles: sqlite3 .backup failed", EXIT_FAILED)
        return [snapshot]
    return sorted(p for p in (home / "worker" / "features").rglob("*") if p.is_file())


def _gc_limit(
    args: argparse.Namespace, cfg: claude_worker.archive_config.ArchiveConfig
) -> float:
    """A NEGATIVE --max-gib means "use the configured cap"; 0 means "empty it"."""
    return cfg.cache_max_gib if args.max_gib < 0 else args.max_gib


def _run_id(raw: str) -> str:
    """Accept a run id or a run directory path."""
    name = pathlib.Path(raw).name
    return name


def _list_row(manifest: dict[str, typing.Any]) -> dict[str, typing.Any]:
    catalog = manifest.get("catalog") or {}
    venues: list[str] = []
    totals = catalog.get("venue_totals") if isinstance(catalog, dict) else None
    if isinstance(totals, dict):
        venues = sorted(k for k, v in totals.items() if isinstance(v, dict) and v.get("ticks"))
    return {
        "run": manifest["run"],
        "epoch_ns": manifest["epoch_ns"],
        "host_id": manifest["host_id"],
        "location": "s3",
        "span_ns": manifest.get("span_ns"),
        "size_bytes": manifest["totals"]["size_bytes"],
        "stored_bytes": manifest["totals"]["stored_bytes"],
        "pmlr_version": manifest.get("pmlr_version"),
        "windows_2h_complete": manifest.get("windows_2h_complete"),
        "venues": venues,
        "local_path": None,
        "uploaded_at_ns": manifest.get("uploaded_at_ns"),
    }


if __name__ == "__main__":
    raise SystemExit(main())
