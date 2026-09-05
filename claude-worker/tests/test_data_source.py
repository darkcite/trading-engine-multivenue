# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Gate G-S5 — the resolver, the cache, and the invariants that protect the disk.

The load-bearing tests here are about what must NOT happen: a partial pull must
not be visible to any discovery law, an unverified file must not survive a pull,
and the cache must not be allowed to fill the volume that retention is trying to
free (the 2026-09-02 ENOSPC wedge stopped every capture lane and did not retry).

`test_disabled_resolver_does_zero_http` is the S-LAW 2 proof for this layer: it
asserts through a request counter, not through a code reading.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import os
import pathlib
import struct
import typing

import pytest

import claude_worker.archive
import claude_worker.archive_config
import claude_worker.data_source
import claude_worker.features
import claude_worker.objstore
import tests.fake_s3

HEADER = struct.Struct("<4sHBxQ")
EPOCH_NS = 1788417289611943000

ENV: typing.Final[dict[str, str]] = {
    "MULTIVENUE_S3_ENABLED": "1",
    "MULTIVENUE_S3_BUCKET": "market-data-qu",
    "MULTIVENUE_S3_ENDPOINT": "fsn1.your-objectstorage.com",
    "MULTIVENUE_S3_REGION": "fsn1",
    "MULTIVENUE_S3_HOST_ID": "mbp-m4",
    "MULTIVENUE_S3_ACCESS_KEY_ID": "AK",
    "MULTIVENUE_S3_SECRET_ACCESS_KEY": "sekrit",
}


def make_pmlr(path: pathlib.Path, *, kind: int, slots: int, start_ns: int) -> None:
    body = bytearray(HEADER.pack(b"PMLR", 3, kind, start_ns))
    body.extend(b"\x00" * (64 - len(body)))
    for i in range(slots):
        slot = bytearray(64)
        slot[0:8] = struct.pack("<Q", start_ns + i * 1_000_000_000)
        body.extend(slot)
    path.write_bytes(bytes(body))


def make_run(root: pathlib.Path, run_id: str) -> pathlib.Path:
    run = root / run_id
    run.mkdir(parents=True)
    start = int(run_id[len("run-") :])
    make_pmlr(run / "pm-ticks.pmlr", kind=0, slots=30, start_ns=start)
    make_pmlr(run / "bn-ticks.pmlr", kind=0, slots=50, start_ns=start)
    (run / "instrument-manifest.tsv").write_text("sym\tvenue\n", encoding="utf-8")
    return run


def make_cfg(
    tmp_path: pathlib.Path, **overrides: str
) -> claude_worker.archive_config.ArchiveConfig:
    # The free-space floor is a property of the REAL volume the tmp_path lives
    # on, and this deployment sits under the production floor today. Tests that
    # are not about the floor therefore disable it; the one that IS about it
    # sets a floor explicitly and fakes the free space.
    env = dict(ENV)
    env["MULTIVENUE_S3_CACHE_DIR"] = str(tmp_path / "cache")
    env["MULTIVENUE_S3_CACHE_MIN_FREE_GIB"] = "0"
    env.update(overrides)
    return claude_worker.archive_config.load(env=env, env_file=tmp_path / "absent.env")


def wire(
    cfg: claude_worker.archive_config.ArchiveConfig, fake: tests.fake_s3.FakeS3
) -> claude_worker.objstore.ObjectStore:
    assert cfg.endpoint is not None and cfg.creds is not None
    return claude_worker.objstore.ObjectStore(
        cfg.endpoint, cfg.creds, fake.client(), timeout_s=5.0, sleep=lambda _s: None
    )


def seeded(
    tmp_path: pathlib.Path, runs: int = 2, **overrides: str
) -> tuple[
    claude_worker.archive_config.ArchiveConfig,
    tests.fake_s3.FakeS3,
    pathlib.Path,
    claude_worker.data_source.DataSource,
]:
    cfg = make_cfg(tmp_path, **overrides)
    fake = tests.fake_s3.FakeS3()
    logs = tmp_path / "logs"
    for i in range(runs + 1):
        make_run(logs, f"run-{EPOCH_NS + i * 10**12}")
    archiver = claude_worker.archive.Archiver(cfg, wire(cfg, fake))
    archiver.push_pending(logs)
    source = claude_worker.data_source.DataSource(cfg, logs, wire(cfg, fake))
    return cfg, fake, logs, source


# --------------------------------------------------------------------------
# discovery
# --------------------------------------------------------------------------


def test_where_reports_each_state(tmp_path: pathlib.Path) -> None:
    _cfg, _fake, logs, source = seeded(tmp_path)
    pushed = f"run-{EPOCH_NS}"
    newest = f"run-{EPOCH_NS + 2 * 10**12}"
    assert source.where(pushed) == "local+s3"
    assert source.where(newest) == "local"  # newest is never uploaded
    assert source.where("run-1") == "absent"

    # Move the local copy aside: it becomes a bucket-only run.
    (logs / pushed).rename(tmp_path / "moved-aside")
    assert source.where(pushed) == "s3"


def test_list_runs_merges_local_cache_and_bucket(tmp_path: pathlib.Path) -> None:
    cfg, _fake, logs, source = seeded(tmp_path)
    pushed = f"run-{EPOCH_NS}"
    (logs / pushed).rename(tmp_path / "moved-aside")

    refs = {r.run_id: r for r in source.list_runs()}
    assert refs[pushed].location == "s3"
    assert refs[pushed].local_path is None
    assert refs[pushed].size_bytes is not None
    assert refs[f"run-{EPOCH_NS + 2 * 10**12}"].location == "local"

    source.ensure_local(pushed)
    refs = {r.run_id: r for r in source.list_runs()}
    assert refs[pushed].location == "cache+s3"
    assert refs[pushed].local_path == cfg.cache_dir / pushed


def test_list_runs_windows_are_the_pool_law_not_an_estimate(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    logs = tmp_path / "logs"
    run = logs / f"run-{EPOCH_NS}"
    run.mkdir(parents=True)
    make_pmlr(run / "pm-ticks.pmlr", kind=0, slots=5 * 3600, start_ns=EPOCH_NS)
    make_run(logs, f"run-{EPOCH_NS + 10**13}")
    claude_worker.archive.Archiver(cfg, wire(cfg, fake)).push_pending(logs)

    source = claude_worker.data_source.DataSource(cfg, logs, wire(cfg, fake))
    ref = next(r for r in source.list_runs() if r.run_id == run.name)
    assert ref.windows_2h_complete == 2


def test_since_and_until_filter_by_epoch(tmp_path: pathlib.Path) -> None:
    _cfg, _fake, _logs, source = seeded(tmp_path, runs=3)
    mid = EPOCH_NS + 10**12
    assert all(r.epoch_ns >= mid for r in source.list_runs(since_ns=mid))
    assert all(r.epoch_ns <= mid for r in source.list_runs(until_ns=mid))


# --------------------------------------------------------------------------
# materialisation
# --------------------------------------------------------------------------


def test_ensure_local_prefers_the_log_root_and_pulls_nothing(
    tmp_path: pathlib.Path,
) -> None:
    _cfg, fake, logs, source = seeded(tmp_path)
    run_id = f"run-{EPOCH_NS}"
    fake.requests.clear()
    assert source.ensure_local(run_id) == logs / run_id
    assert fake.count("GET") == 0, "a local run must never be downloaded"


def test_ensure_local_pulls_and_verifies_every_sha256(tmp_path: pathlib.Path) -> None:
    cfg, _fake, logs, source = seeded(tmp_path)
    run_id = f"run-{EPOCH_NS}"
    original = {p.name: p.read_bytes() for p in (logs / run_id).iterdir()}
    (logs / run_id).rename(tmp_path / "moved-aside")

    pulled = source.ensure_local(run_id)
    assert pulled == cfg.cache_dir / run_id
    for name, blob in original.items():
        assert (pulled / name).read_bytes() == blob, name


def test_pull_with_a_corrupted_object_leaves_nothing_visible(
    tmp_path: pathlib.Path,
) -> None:
    cfg, fake, logs, source = seeded(tmp_path)
    run_id = f"run-{EPOCH_NS}"
    (logs / run_id).rename(tmp_path / "moved-aside")

    # Same length, different bytes: size and ETag checks pass, the sha256 of the
    # decompressed payload is the only thing that can catch this.
    manifest = json.loads(fake.objects[cfg.manifest_key(run_id)])
    entry = manifest["files"][0]
    fake.objects[entry["key"]] = b"\x00" * int(entry["stored_bytes"])

    with pytest.raises(Exception):
        source.ensure_local(run_id)
    assert not (cfg.cache_dir / run_id).exists()
    assert claude_worker.features.run_dirs(cfg.cache_dir) == []


def test_partial_dir_is_invisible_to_every_discovery_law(tmp_path: pathlib.Path) -> None:
    """The name is the guard: `run-<ns>.partial` fails the `isdigit()` suffix
    test that features.run_dirs and Rust's parse_run_dir_name both apply."""
    cache = tmp_path / "cache"
    cache.mkdir()
    partial = cache / f"run-{EPOCH_NS}.partial"
    partial.mkdir()
    make_pmlr(partial / "pm-ticks.pmlr", kind=0, slots=5, start_ns=EPOCH_NS)
    assert claude_worker.features.run_dirs(cache) == []

    cfg = make_cfg(tmp_path)
    source = claude_worker.data_source.DataSource(cfg, tmp_path / "logs", None)
    assert source.cached_runs() == {}


def test_cache_entry_without_its_marker_is_not_served(tmp_path: pathlib.Path) -> None:
    """A directory with the right name but no completion marker is a half-truth,
    and a half-truth is worse than an absence."""
    cfg, _fake, logs, source = seeded(tmp_path)
    run_id = f"run-{EPOCH_NS}"
    (logs / run_id).rename(tmp_path / "moved-aside")
    source.ensure_local(run_id)
    (cfg.cache_dir / f".{run_id}.complete").unlink()
    assert source.cached_runs() == {}


# --------------------------------------------------------------------------
# cache policy — the disk is the thing being protected
# --------------------------------------------------------------------------


def test_lru_evicts_by_complete_marker_mtime(tmp_path: pathlib.Path) -> None:
    cfg, _fake, logs, source = seeded(tmp_path, runs=3)
    ids = [f"run-{EPOCH_NS + i * 10**12}" for i in range(3)]
    for run_id in ids:
        (logs / run_id).rename(tmp_path / f"aside-{run_id}")
        source.ensure_local(run_id)

    # Make the FIRST one the least recently used.
    for offset, run_id in enumerate(ids):
        marker = cfg.cache_dir / f".{run_id}.complete"
        stamp = 1_000_000.0 + offset
        os.utime(marker, (stamp, stamp))

    evicted = source._evict_to(0)
    assert evicted == 3
    assert claude_worker.features.run_dirs(cfg.cache_dir) == []


def test_free_space_floor_refuses_the_pull(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cfg, _fake, logs, source = seeded(
        tmp_path, runs=2, MULTIVENUE_S3_CACHE_MIN_FREE_GIB="25"
    )
    run_id = f"run-{EPOCH_NS}"
    (logs / run_id).rename(tmp_path / "moved-aside")
    monkeypatch.setattr(claude_worker.archive, "free_gib", lambda _p: 1.0)
    with pytest.raises(claude_worker.data_source.DataSourceError, match="floor"):
        source.ensure_local(run_id)
    assert not (cfg.cache_dir / run_id).exists()


def test_eviction_runs_before_the_floor_is_judged(tmp_path: pathlib.Path) -> None:
    """Refusing before evicting would strand a cache full of runs nobody wants."""
    _cfg, _fake, logs, source = seeded(tmp_path, runs=2, MULTIVENUE_S3_CACHE_MAX_GIB="0")
    ids = [f"run-{EPOCH_NS + i * 10**12}" for i in range(2)]
    for run_id in ids:
        (logs / run_id).rename(tmp_path / f"aside-{run_id}")
        source.ensure_local(run_id)
    # With a zero cap every pull evicts what came before it, so the cache holds
    # at most the run just fetched — and the floor is never even consulted.
    assert len(source.cached_runs()) <= 1
    source._make_room(0)
    assert source.cached_runs() == {}


def test_release_only_ages_the_marker_and_never_deletes(tmp_path: pathlib.Path) -> None:
    cfg, _fake, logs, source = seeded(tmp_path)
    run_id = f"run-{EPOCH_NS}"
    (logs / run_id).rename(tmp_path / "moved-aside")
    source.ensure_local(run_id)
    before = (cfg.cache_dir / f".{run_id}.complete").stat().st_mtime
    source.release(run_id)
    after = (cfg.cache_dir / f".{run_id}.complete").stat().st_mtime
    assert after < before
    assert (cfg.cache_dir / run_id).is_dir(), "release must never delete"


def test_gc_removes_partials_and_respects_the_cap(tmp_path: pathlib.Path) -> None:
    cfg, _fake, _logs, _source = seeded(tmp_path)
    (cfg.cache_dir / f"run-{EPOCH_NS}.partial").mkdir(parents=True, exist_ok=True)
    removed = claude_worker.archive.gc_cache(cfg, cfg.cache_max_gib)
    assert removed["partial"] == 1
    assert not (cfg.cache_dir / f"run-{EPOCH_NS}.partial").exists()


# --------------------------------------------------------------------------
# disabled + provenance
# --------------------------------------------------------------------------


def test_disabled_resolver_does_zero_http(tmp_path: pathlib.Path) -> None:
    """S-LAW 2 for this layer: no store is constructed, so no request is possible."""
    cfg = claude_worker.archive_config.load(env={}, env_file=tmp_path / "absent.env")
    logs = tmp_path / "logs"
    make_run(logs, f"run-{EPOCH_NS}")
    source = claude_worker.data_source.build(cfg, logs)
    assert source.store is None
    assert [r.run_id for r in source.list_runs()] == [f"run-{EPOCH_NS}"]
    assert source.where(f"run-{EPOCH_NS}") == "local"
    assert source.ensure_local(f"run-{EPOCH_NS}") == logs / f"run-{EPOCH_NS}"


def test_disabled_resolver_cannot_materialise_an_absent_run(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env={}, env_file=tmp_path / "absent.env")
    source = claude_worker.data_source.build(cfg, tmp_path / "logs")
    with pytest.raises(claude_worker.data_source.DataSourceError, match="disabled"):
        source.ensure_local(f"run-{EPOCH_NS}")


def test_provenance_block_matches_where(tmp_path: pathlib.Path) -> None:
    _cfg, _fake, _logs, source = seeded(tmp_path)
    pushed = f"run-{EPOCH_NS}"
    newest = f"run-{EPOCH_NS + 2 * 10**12}"
    block = source.provenance([pushed, newest])
    assert block["archive_enabled"] is True
    assert block["runs"][0] == {
        "run": pushed,
        "location": "local+s3",
        "verified_sha256": None,
    }
    assert block["runs"][1]["location"] == "local"


def test_disabled_provenance_block_is_the_empty_shape(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env={}, env_file=tmp_path / "absent.env")
    source = claude_worker.data_source.build(cfg, tmp_path / "logs")
    assert source.provenance([]) == {"archive_enabled": False, "runs": []}


def test_pulled_run_records_the_manifest_it_was_verified_against(
    tmp_path: pathlib.Path,
) -> None:
    """S-LAW 8: a number produced from archived data must be traceable to the
    exact bytes it came from."""
    cfg, fake, logs, source = seeded(tmp_path)
    run_id = f"run-{EPOCH_NS}"
    (logs / run_id).rename(tmp_path / "moved-aside")
    source.ensure_local(run_id)
    expected = claude_worker.data_source.manifest_sha256(
        json.loads(fake.objects[cfg.manifest_key(run_id)])
    )
    assert source._verified_sha(run_id) == expected
    block = source.provenance([run_id])
    assert block["runs"][0]["verified_sha256"] == expected
