# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The resolver: where is this run, and give me a local path for it (phase S5).

This is the only new concept the rest of the codebase sees, and it deliberately
hands back a ``pathlib.Path`` — which is what every consumer already accepts.
Rust's ``discover_runs`` and Python's ``run_dirs`` / ``select_runs`` gate on
``is_dir()`` plus a ``run-<digits>`` name and are indifferent to origin, so
cache-on-demand has a blast radius of zero: nothing downstream learns that a
run came from a bucket.

Three rules keep that promise honest:

- **A partial pull is invisible.** Downloads assemble under a ``.partial``
  suffix, which breaks the ``run-<digits>`` name every discovery law keys on. An
  interrupted pull is therefore not a truncated run that some backtest quietly
  reads — it is nothing at all, and ``gc`` sweeps it.
- **Nothing is trusted until it is verified.** Every pulled file's ORIGINAL
  sha256 is checked after decompression, before the directory takes its real
  name.
- **The cache never wins a fight with the disk.** The volume ENOSPC-wedged
  every capture lane once, and writers do not retry after ENOSPC. So the free
  floor is checked BEFORE a pull, not after, and eviction happens first.

The cache is disposable by construction (S-LAW 9): the bucket holds everything
under it, so it can be deleted at any moment without loss. ``gc`` touches only
the cache — never the bucket, never the log root.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import hashlib
import json
import os
import pathlib
import shutil
import typing

import httpx

import claude_worker.archive
import claude_worker.archive_config
import claude_worker.features
import claude_worker.objstore
import claude_worker.window_root

Location = typing.Literal["local", "local+s3", "cache+s3", "s3", "absent"]

GIB: typing.Final[int] = 1024**3


class DataSourceError(Exception):
    """A run could not be resolved to a local path."""


class RunRef(typing.NamedTuple):
    run_id: str
    epoch_ns: int
    location: Location
    local_path: pathlib.Path | None
    span_ns: tuple[int, int] | None
    size_bytes: int | None
    stored_bytes: int | None
    pmlr_version: int | None
    windows_2h_complete: int | None
    venues: tuple[str, ...]
    verified_sha256: str | None


def _epoch_of(run_id: str) -> int:
    suffix = run_id[len("run-") :]
    return int(suffix) if suffix.isdigit() else 0


def _manifest_sizes(
    manifest: dict[str, typing.Any] | None,
) -> tuple[int | None, int | None]:
    """(size_bytes, stored_bytes) from a manifest of EITHER shape.

    A run manifest carries a `totals` block. A legacy tarball manifest carries
    only `{size_bytes, sha256}` — the richer facts did not exist when retention
    wrote the tarball, and inventing them would mean extracting every archive
    just to list it. A tarball is already compressed, so stored == size.
    """
    if manifest is None:
        return None, None
    totals = manifest.get("totals")
    if totals:
        return totals.get("size_bytes"), totals.get("stored_bytes")
    size = manifest.get("size_bytes")
    return size, size


def _venues_from_catalog(manifest: dict[str, typing.Any]) -> tuple[str, ...]:
    catalog = manifest.get("catalog")
    if not isinstance(catalog, dict):
        return ()
    totals = catalog.get("venue_totals")
    if not isinstance(totals, dict):
        return ()
    # venue_totals, not the per-day venue_ticks array: the latter iterates 6 of
    # a 7-wide venue list and silently omits bybit (capture_catalog.rs).
    return tuple(sorted(k for k, v in totals.items() if isinstance(v, dict) and v.get("ticks")))


class DataSource:
    def __init__(
        self,
        cfg: claude_worker.archive_config.ArchiveConfig,
        replay_dir: pathlib.Path,
        store: claude_worker.objstore.ObjectStore | None,
    ) -> None:
        self.cfg = cfg
        self.replay_dir = replay_dir
        self.store = store
        self._archiver = (
            claude_worker.archive.Archiver(cfg, store) if store is not None else None
        )

    # -- discovery -------------------------------------------------------

    def local_runs(self) -> dict[str, pathlib.Path]:
        # features.run_dirs is CALLED, never modified: 15 callers depend on it
        # and the newest run is always local anyway.
        return {p.name: p for p in claude_worker.features.run_dirs(self.replay_dir)}

    def cached_runs(self) -> dict[str, pathlib.Path]:
        """Complete cache entries only — a `.partial` has no `run-<digits>` name."""
        out: dict[str, pathlib.Path] = {}
        if not self.cfg.cache_dir.is_dir():
            return out
        for path in claude_worker.features.run_dirs(self.cfg.cache_dir):
            if self._marker(path.name).is_file():
                out[path.name] = path
        return out

    def remote_manifests(self) -> dict[str, dict[str, typing.Any]]:
        """Runs held in the bucket, whichever shape they are in.

        Directory archives win over legacy tarballs for the same run id: a
        directory can be windowed in place, a tarball has to be extracted first.
        """
        if self._archiver is None:
            return {}
        out: dict[str, dict[str, typing.Any]] = dict(self._archiver.list_tarballs())
        out.update({m["run"]: m for m in self._archiver.list_runs()})
        return out

    def _remote_shape(self, run_id: str) -> str | None:
        """``"run"``, ``"tarball"`` or None — how the bucket holds this run."""
        if self.store is None or self._archiver is None:
            return None
        if self.store.head(self.cfg.index_key(run_id)) is not None:
            return "run"
        if self.store.head(self._archiver.tarball_index_key(run_id)) is not None:
            return "tarball"
        return None

    def where(self, run_id: str) -> Location:
        local = run_id in self.local_runs()
        cached = run_id in self.cached_runs()
        remote = self._remote_shape(run_id) is not None
        if local:
            return "local+s3" if remote else "local"
        if cached and remote:
            return "cache+s3"
        if cached:
            # In the bucket's absence a cache entry is still a real directory,
            # but it is no longer backed by anything — report it as local so
            # nobody deletes it thinking a copy exists.
            return "local"
        if remote:
            return "s3"
        return "absent"

    def list_runs(
        self, since_ns: int | None = None, until_ns: int | None = None
    ) -> list[RunRef]:
        local = self.local_runs()
        cached = self.cached_runs()
        manifests = self.remote_manifests()

        refs: dict[str, RunRef] = {}
        for run_id, path in list(local.items()) + list(cached.items()):
            manifest = manifests.get(run_id)
            size, stored = _manifest_sizes(manifest)
            span = claude_worker.window_root.run_span(path)
            refs[run_id] = RunRef(
                run_id=run_id,
                epoch_ns=_epoch_of(run_id),
                location=(
                    ("local+s3" if manifest else "local")
                    if run_id in local
                    else ("cache+s3" if manifest else "local")
                ),
                local_path=path,
                span_ns=span,
                size_bytes=size,
                stored_bytes=stored,
                pmlr_version=claude_worker.window_root.run_pmlr_version(path),
                windows_2h_complete=len(claude_worker.window_root.complete_windows(path)),
                venues=_venues_from_catalog(manifest) if manifest else (),
                verified_sha256=self._verified_sha(run_id) if run_id in cached else None,
            )
        for run_id, manifest in manifests.items():
            if run_id in refs:
                continue
            span = manifest.get("span_ns")
            size, stored = _manifest_sizes(manifest)
            refs[run_id] = RunRef(
                run_id=run_id,
                epoch_ns=manifest.get("epoch_ns") or _epoch_of(run_id),
                location="s3",
                local_path=None,
                span_ns=(span[0], span[1]) if span else None,
                size_bytes=size,
                stored_bytes=stored,
                pmlr_version=manifest.get("pmlr_version"),
                windows_2h_complete=manifest.get("windows_2h_complete"),
                venues=_venues_from_catalog(manifest),
                verified_sha256=None,
            )

        out = sorted(refs.values(), key=lambda r: r.epoch_ns)
        if since_ns is not None:
            out = [r for r in out if r.epoch_ns >= since_ns]
        if until_ns is not None:
            out = [r for r in out if r.epoch_ns <= until_ns]
        return out

    # -- materialisation -------------------------------------------------

    def _marker(self, run_id: str) -> pathlib.Path:
        return self.cfg.cache_dir / f".{run_id}.complete"

    def _verified_sha(self, run_id: str) -> str | None:
        try:
            data = json.loads(self._marker(run_id).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        value = data.get("manifest_sha256")
        return str(value) if value else None

    def ensure_local(self, run_id: str) -> pathlib.Path:
        """A real local path for ``run_id``, pulling it only if it must."""
        local = self.local_runs().get(run_id)
        if local is not None:
            return local

        cached = self.cfg.cache_dir / run_id
        if cached.is_dir() and self._marker(run_id).is_file():
            # touch the marker: it is the LRU clock. APFS atime is not trusted.
            self._marker(run_id).touch()
            return cached

        if self._archiver is None or self.store is None:
            raise DataSourceError(f"{run_id}: not local and the archive is disabled")

        # A directory archive is preferred; a legacy tarball is the fallback,
        # because the oldest week of history only exists in that shape.
        shape = self._remote_shape(run_id)
        if shape == "run":
            manifest = self._archiver.verify(run_id)
            self._make_room(int(manifest["totals"]["size_bytes"]))
            return self._archiver.pull(run_id, self.cfg.cache_dir)

        if shape == "tarball":
            manifest = self._archiver.verify_tarball(run_id)
            # The tarball AND its expansion coexist on disk during extraction,
            # so budget for both rather than for the compressed size alone.
            self._make_room(int(manifest["size_bytes"]) * 6)
            return self._archiver.pull_tarball(run_id, self.cfg.cache_dir)

        raise DataSourceError(f"{run_id}: not local and not in the archive")

    def _make_room(self, needed_bytes: int) -> None:
        """Evict by LRU, then refuse if the volume still could not take it.

        Order matters: evicting first can be what makes the pull possible, and
        refusing first would strand a cache full of runs nobody wants.
        """
        self._evict_to(self.cfg.cache_max_gib * GIB - needed_bytes)
        free = claude_worker.archive.free_gib(self.cfg.cache_dir)
        if free - needed_bytes / GIB < self.cfg.cache_min_free_gib:
            raise DataSourceError(
                f"cache: free space floor — {free:.1f} GiB free, "
                f"{needed_bytes / GIB:.1f} GiB needed, "
                f"{self.cfg.cache_min_free_gib} GiB floor"
            )

    def _cache_entries(self) -> list[tuple[float, pathlib.Path, int]]:
        out: list[tuple[float, pathlib.Path, int]] = []
        for path in claude_worker.features.run_dirs(self.cfg.cache_dir):
            marker = self._marker(path.name)
            if not marker.is_file():
                continue
            size = sum(f.stat().st_size for f in path.rglob("*") if f.is_file())
            out.append((marker.stat().st_mtime, path, size))
        out.sort(key=lambda row: row[0])  # oldest touch first
        return out

    def _evict_to(self, limit_bytes: float) -> int:
        entries = self._cache_entries()
        total = sum(size for _m, _p, size in entries)
        evicted = 0
        for _mtime, path, size in entries:
            if total <= limit_bytes:
                break
            shutil.rmtree(path, ignore_errors=True)
            self._marker(path.name).unlink(missing_ok=True)
            total -= size
            evicted += 1
        return evicted

    def release(self, run_id: str) -> None:
        """An LRU hint, nothing more. Never deletes: the caller may still be
        holding a path into this directory."""
        marker = self._marker(run_id)
        if marker.is_file():
            stamp = marker.stat().st_mtime - 10_000.0
            os.utime(marker, (stamp, stamp))

    # -- provenance ------------------------------------------------------

    def provenance(self, run_ids: collections.abc.Sequence[str]) -> dict[str, typing.Any]:
        """The additive block a report stamps so a number can be traced to
        the bytes it came from (S-LAW 8)."""
        rows: list[dict[str, typing.Any]] = []
        for run_id in run_ids:
            rows.append(
                {
                    "run": run_id,
                    "location": self.where(run_id) if self.store is not None else "local",
                    "verified_sha256": self._verified_sha(run_id),
                }
            )
        return {
            "archive_enabled": claude_worker.archive_config.is_enabled(self.cfg),
            "runs": rows,
        }


def build(
    cfg: claude_worker.archive_config.ArchiveConfig,
    replay_dir: pathlib.Path,
    *,
    client: httpx.Client | None = None,
) -> DataSource:
    """A resolver that does ZERO HTTP when the subsystem is disabled.

    The store is simply not constructed, so there is no client that could be
    called by accident — the guarantee is structural, not a flag someone has to
    remember to check.
    """
    if not claude_worker.archive_config.is_enabled(cfg):
        return DataSource(cfg, replay_dir, None)
    assert cfg.endpoint is not None and cfg.creds is not None
    store = claude_worker.objstore.ObjectStore(
        cfg.endpoint,
        cfg.creds,
        client if client is not None else httpx.Client(),
        timeout_s=cfg.timeout_s,
    )
    return DataSource(cfg, replay_dir, store)


def manifest_sha256(manifest: dict[str, typing.Any]) -> str:
    return hashlib.sha256(claude_worker.archive.manifest_bytes(manifest)).hexdigest()
