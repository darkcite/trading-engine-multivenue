# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Gate G-S3 — round trip, resume, and the commit order that makes deletion safe.

The tests that matter here are the ones about failure. `retention.sh` deletes a
local capture run on the strength of `verify` returning 0, so the interesting
question is never "does a clean upload work" — it is "what does a torn upload
look like to a reader". The answer must always be: nothing. No manifest, no
index, no visible run.

Every test runs against `tests/fake_s3.py` over httpx.MockTransport. No socket.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import random
import struct
import subprocess
import typing

import pytest

import claude_worker.archive
import claude_worker.archive_config
import claude_worker.objstore
import claude_worker.window_root
import tests.fake_s3

HEADER = struct.Struct("<4sHBxQ")
SLOT = 64
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


def make_pmlr(
    path: pathlib.Path, *, kind: int, slots: int, start_ns: int = EPOCH_NS
) -> None:
    """A structurally valid PMLR v3 file. `slots=0` is header-only (64 B)."""
    body = bytearray(HEADER.pack(b"PMLR", 3, kind, start_ns))
    body.extend(b"\x00" * (64 - len(body)))
    for i in range(slots):
        slot = bytearray(SLOT)
        slot[0:8] = struct.pack("<Q", start_ns + i * 1_000_000_000)
        body.extend(slot)
    path.write_bytes(bytes(body))


def make_run(root: pathlib.Path, run_id: str, *, big_slots: int = 0) -> pathlib.Path:
    """A run dir shaped like a real one: ticks, events, header-only files, TSVs."""
    run = root / run_id
    run.mkdir(parents=True)
    start = int(run_id[len("run-") :])
    make_pmlr(run / "pm-ticks.pmlr", kind=0, slots=40, start_ns=start)
    make_pmlr(run / "bn-ticks.pmlr", kind=0, slots=big_slots or 60, start_ns=start)
    make_pmlr(run / "okx-depth.pmlr", kind=7, slots=5, start_ns=start)
    make_pmlr(run / "ai-cmds.pmlr", kind=4, slots=2, start_ns=start)
    for name in ("deribit-signals.pmlr", "hl-events.pmlr", "bybit-opt-summary.pmlr"):
        make_pmlr(run / name, kind=1, slots=0, start_ns=start)  # header-only, 64 B
    (run / "instrument-manifest.tsv").write_text("sym\tvenue\n7\tbn\n", encoding="utf-8")
    (run / "options-manifest.tsv").write_text("sym\tstrike\n", encoding="utf-8")
    return run


def make_cfg(
    tmp_path: pathlib.Path, **overrides: str
) -> claude_worker.archive_config.ArchiveConfig:
    env = dict(ENV, MULTIVENUE_S3_CACHE_DIR=str(tmp_path / "cache"), **overrides)
    return claude_worker.archive_config.load(env=env, env_file=tmp_path / "absent.env")


def make_archiver(
    cfg: claude_worker.archive_config.ArchiveConfig, fake: tests.fake_s3.FakeS3
) -> claude_worker.archive.Archiver:
    assert cfg.endpoint is not None and cfg.creds is not None
    store = claude_worker.objstore.ObjectStore(
        cfg.endpoint, cfg.creds, fake.client(), timeout_s=5.0, sleep=lambda _s: None
    )
    return claude_worker.archive.Archiver(cfg, store)


# --------------------------------------------------------------------------
# round trip
# --------------------------------------------------------------------------


def test_round_trip_byte_identical(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")  # a newer run, so `run` is closed

    result = archiver.push_run(run, with_catalog=False)
    assert result.files == 9

    into = tmp_path / "pulled"
    pulled = archiver.pull(run.name, into)
    for original in sorted(run.iterdir()):
        assert (pulled / original.name).read_bytes() == original.read_bytes(), original.name
    assert sorted(p.name for p in pulled.iterdir()) == sorted(p.name for p in run.iterdir())


def test_manifest_and_index_are_the_same_bytes(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    archiver.push_run(run, with_catalog=False)
    assert fake.objects[cfg.manifest_key(run.name)] == fake.objects[cfg.index_key(run.name)]


def test_index_is_written_last(tmp_path: pathlib.Path) -> None:
    """S-LAW 5: the index object is the completeness truth, so it must be the
    last thing written — everything before it is invisible to every reader."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    archiver.push_run(run, with_catalog=False)
    order = fake.put_keys
    assert order[-1] == cfg.index_key(run.name)
    assert order[-2] == cfg.manifest_key(run.name)
    assert all(k not in (cfg.index_key(run.name),) for k in order[:-1])


def test_windows_2h_complete_matches_window_root(tmp_path: pathlib.Path) -> None:
    """The manifest's window count must equal the pool law's, not approximate it."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = logs / f"run-{EPOCH_NS}"
    run.mkdir(parents=True)
    # 5 hours of ticks at 1 s -> exactly 2 complete 2 h windows plus a tail.
    make_pmlr(run / "pm-ticks.pmlr", kind=0, slots=5 * 3600, start_ns=EPOCH_NS)
    make_run(logs, f"run-{EPOCH_NS + 10**13}")

    archiver.push_run(run, with_catalog=False)
    manifest = json.loads(fake.objects[cfg.manifest_key(run.name)])
    assert manifest["windows_2h_complete"] == len(
        claude_worker.window_root.complete_windows(run)
    )
    assert manifest["windows_2h_complete"] == 2
    assert manifest["pmlr_version"] == 3
    assert manifest["span_ns"] == list(claude_worker.window_root.run_span(run) or ())


def test_header_only_files_are_pushed_with_null_pmlr_facts(tmp_path: pathlib.Path) -> None:
    """20 of a run's ~37 files are normally 64 B headers. They are data too."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    archiver.push_run(run, with_catalog=False)
    manifest = json.loads(fake.objects[cfg.manifest_key(run.name)])
    header_only = next(f for f in manifest["files"] if f["name"] == "hl-events.pmlr")
    assert header_only["size_bytes"] == 64
    assert header_only["slots"] == 0
    assert header_only["first_ts_ns"] is None
    tsv = next(f for f in manifest["files"] if f["name"] == "instrument-manifest.tsv")
    assert tsv["pmlr_version"] is None


def test_multipart_path_is_used_for_large_files(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path, MULTIVENUE_S3_PART_SIZE_MIB="5", MULTIVENUE_S3_ZSTD_LEVEL="1")
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = logs / f"run-{EPOCH_NS}"
    run.mkdir(parents=True)
    # Incompressible, so the STORED object is comfortably over the part size.
    rng = random.Random(7)
    (run / "bn-raw.tap").write_bytes(rng.randbytes(12 * 1024 * 1024))
    make_pmlr(run / "pm-ticks.pmlr", kind=0, slots=10)
    make_run(logs, f"run-{EPOCH_NS + 10**12}")

    result = archiver.push_run(run, with_catalog=False)
    assert result.parts > 1
    manifest = json.loads(fake.objects[cfg.manifest_key(run.name)])
    tap = next(f for f in manifest["files"] if f["name"] == "bn-raw.tap")
    assert tap["parts"] > 1
    assert fake.objects[tap["key"]]


# --------------------------------------------------------------------------
# failure and resume
# --------------------------------------------------------------------------


def test_resume_after_kill_skips_verified_files_and_left_nothing_visible(
    tmp_path: pathlib.Path,
) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")

    fake.fail_after_n_puts = 4
    with pytest.raises(claude_worker.objstore.ObjStoreError):
        archiver.push_run(run, with_catalog=False)

    # Nothing a reader consults exists yet.
    assert cfg.manifest_key(run.name) not in fake.objects
    assert cfg.index_key(run.name) not in fake.objects
    first_pass = list(fake.put_keys)
    assert len(first_pass) == 4

    fake.fail_after_n_puts = None
    fake.put_keys.clear()
    archiver.push_run(run, with_catalog=False)

    # The four already-uploaded objects were not sent again.
    for key in first_pass:
        assert key not in fake.put_keys, f"{key} was re-uploaded"
    assert cfg.index_key(run.name) in fake.objects


def test_torn_object_with_wrong_size_is_overwritten(tmp_path: pathlib.Path) -> None:
    """The one narrow overwrite S-LAW 11 permits: a previous attempt's torn
    object, identified by a size that disagrees with what we are about to send."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    fake.objects[cfg.run_key(run.name, "pm-ticks.pmlr")] = b"torn"

    archiver.push_run(run, with_catalog=False)
    assert fake.objects[cfg.run_key(run.name, "pm-ticks.pmlr")] != b"torn"
    archiver.verify(run.name)


def test_etag_mismatch_leaves_no_manifest_and_no_index(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    fake.corrupt_etag.add(cfg.run_key(f"run-{EPOCH_NS}", "pm-ticks.pmlr"))
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")

    with pytest.raises(claude_worker.objstore.DigestMismatch):
        archiver.push_run(run, with_catalog=False)
    assert cfg.manifest_key(run.name) not in fake.objects
    assert cfg.index_key(run.name) not in fake.objects


def test_push_is_idempotent_once_the_index_exists(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    archiver.push_run(run, with_catalog=False)

    fake.put_keys.clear()
    again = archiver.push_run(run, with_catalog=False)
    assert again.skipped
    assert fake.put_keys == [], "an already-complete run must not re-upload a byte"


def test_manifest_without_index_is_finished_not_reuploaded(tmp_path: pathlib.Path) -> None:
    """The crash window between the two final writes. Finish it, cheaply."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    archiver.push_run(run, with_catalog=False)
    del fake.objects[cfg.index_key(run.name)]

    fake.put_keys.clear()
    result = archiver.push_run(run, with_catalog=False)
    assert result.skipped
    assert fake.put_keys == [cfg.index_key(run.name)]
    assert fake.objects[cfg.index_key(run.name)] == fake.objects[cfg.manifest_key(run.name)]


def test_budget_spent_exits_5_and_is_resumable(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")

    ticks = iter([0.0] * 3 + [10_000.0] * 40)
    budget = claude_worker.objstore.Budget(deadline=1.0, clock=lambda: next(ticks))
    with pytest.raises(claude_worker.archive.ArchiveError) as caught:
        archiver.push_run(run, budget=budget, with_catalog=False)
    assert caught.value.code == claude_worker.archive.EXIT_BUDGET
    assert cfg.manifest_key(run.name) not in fake.objects
    assert cfg.index_key(run.name) not in fake.objects

    archiver.push_run(run, with_catalog=False)  # resumes cleanly
    archiver.verify(run.name)


# --------------------------------------------------------------------------
# eligibility (S-LAW 4)
# --------------------------------------------------------------------------


def test_newest_run_is_refused_without_force(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    make_run(logs, f"run-{EPOCH_NS}")
    newest = make_run(logs, f"run-{EPOCH_NS + 10**12}")
    with pytest.raises(claude_worker.archive.ArchiveError) as caught:
        archiver.push_run(newest, with_catalog=False)
    assert caught.value.code == claude_worker.archive.EXIT_REFUSED
    assert fake.count("PUT") == 0


def test_force_on_the_newest_run_is_refused_while_files_are_fresh(
    tmp_path: pathlib.Path,
) -> None:
    logs = tmp_path / "logs"
    make_run(logs, f"run-{EPOCH_NS}")
    newest = make_run(logs, f"run-{EPOCH_NS + 10**12}")
    with pytest.raises(claude_worker.archive.ArchiveError) as caught:
        claude_worker.archive.assert_eligible(newest, force=True)
    assert caught.value.code == claude_worker.archive.EXIT_REFUSED


def test_push_pending_never_includes_the_newest_run(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    for offset in (0, 10**12, 2 * 10**12):
        make_run(logs, f"run-{EPOCH_NS + offset}")
    results = archiver.push_pending(logs)
    assert [r.run for r in results] == [f"run-{EPOCH_NS}", f"run-{EPOCH_NS + 10**12}"]
    assert cfg.index_key(f"run-{EPOCH_NS + 2 * 10**12}") not in fake.objects


def test_free_space_floor_refuses_and_touches_nothing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    monkeypatch.setattr(claude_worker.archive, "free_gib", lambda _p: 0.5)
    with pytest.raises(claude_worker.archive.ArchiveError) as caught:
        archiver.push_run(run, with_catalog=False)
    assert caught.value.code == claude_worker.archive.EXIT_NO_SPACE
    assert fake.count("PUT") == 0


# --------------------------------------------------------------------------
# verify + catalog + disabled
# --------------------------------------------------------------------------


def test_verify_exit_codes(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")

    with pytest.raises(claude_worker.archive.ArchiveError) as missing:
        archiver.verify(run.name)
    assert missing.value.code == claude_worker.archive.EXIT_FAILED

    archiver.push_run(run, with_catalog=False)
    archiver.verify(run.name)
    archiver.verify(run.name, deep=True)

    # An object vanishing from under a good manifest must be caught.
    manifest = json.loads(fake.objects[cfg.manifest_key(run.name)])
    del fake.objects[manifest["files"][0]["key"]]
    with pytest.raises(claude_worker.archive.ArchiveError):
        archiver.verify(run.name)


def test_verify_rejects_a_size_that_disagrees_with_the_manifest(
    tmp_path: pathlib.Path,
) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    archiver.push_run(run, with_catalog=False)
    manifest = json.loads(fake.objects[cfg.manifest_key(run.name)])
    fake.objects[manifest["files"][0]["key"]] = b"shorter"
    with pytest.raises(claude_worker.archive.ArchiveError, match="size"):
        archiver.verify(run.name)


def test_verify_rejects_index_and_manifest_that_differ(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")
    archiver.push_run(run, with_catalog=False)
    fake.objects[cfg.index_key(run.name)] = b'{"archive_manifest_version": 1}'
    with pytest.raises(claude_worker.archive.ArchiveError, match="differ"):
        archiver.verify(run.name)


def test_catalog_absent_is_null_not_a_failure(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    make_run(logs, f"run-{EPOCH_NS + 10**12}")

    def no_binary(*_a: object, **_k: object) -> typing.NoReturn:
        raise OSError("multivenue-engine not on PATH")

    monkeypatch.setattr(claude_worker.archive.subprocess, "run", no_binary)
    archiver.push_run(run, with_catalog=True)
    manifest = json.loads(fake.objects[cfg.manifest_key(run.name)])
    assert manifest["catalog"] is None
    assert manifest["catalog_error"] == "absent"


def test_disabled_gating_lanes_exit_3_and_do_zero_http(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env={}, env_file=tmp_path / "absent.env")
    assert not cfg.enabled
    with pytest.raises(claude_worker.archive.ArchiveError) as caught:
        claude_worker.archive.build(cfg)
    assert caught.value.code == claude_worker.archive.EXIT_DISABLED


def test_pull_partial_dir_is_invisible_to_every_discovery_law(
    tmp_path: pathlib.Path,
) -> None:
    """An interrupted pull must not be served as a truncated run."""
    partial = tmp_path / f"run-{EPOCH_NS}.partial"
    partial.mkdir()
    assert claude_worker.features.run_dirs(tmp_path) == []


def test_list_runs_returns_one_row_per_complete_run(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    for offset in (0, 10**12, 2 * 10**12):
        make_run(logs, f"run-{EPOCH_NS + offset}")
    archiver.push_pending(logs)
    rows = archiver.list_runs()
    assert [r["run"] for r in rows] == [f"run-{EPOCH_NS}", f"run-{EPOCH_NS + 10**12}"]
    row = claude_worker.archive._list_row(rows[0])
    assert row["location"] == "s3"
    assert row["pmlr_version"] == 3
    assert row["windows_2h_complete"] == 0


# --------------------------------------------------------------------------
# the key schema
# --------------------------------------------------------------------------


def test_tarball_round_trip_extracts_the_original_run(tmp_path: pathlib.Path) -> None:
    """The oldest week of history exists ONLY as legacy tarballs, so a tarball
    that cannot be pulled back is write-only storage."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    original = {p.name: p.read_bytes() for p in run.iterdir()}

    # exactly the shape retention.sh writes: tar -czf <out> -C <root> <name>
    tarball = tmp_path / f"{run.name}.tar.gz"
    subprocess.run(
        ["tar", "-czf", str(tarball), "-C", str(logs), run.name], check=True
    )
    result = archiver.push_tarball(tarball)
    assert result.files == 1
    assert cfg.tarballs_prefix() + run.name + ".tar.gz" in fake.objects

    archiver.verify_tarball(run.name)
    pulled = archiver.pull_tarball(run.name, tmp_path / "cache2")
    assert pulled.name == run.name
    assert {p.name: p.read_bytes() for p in pulled.iterdir()} == original


def test_tarball_push_is_idempotent(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    tarball = tmp_path / f"{run.name}.tar.gz"
    subprocess.run(["tar", "-czf", str(tarball), "-C", str(logs), run.name], check=True)
    archiver.push_tarball(tarball)
    fake.put_keys.clear()
    assert archiver.push_tarball(tarball).skipped
    assert fake.put_keys == []


def test_corrupt_tarball_is_never_extracted(tmp_path: pathlib.Path) -> None:
    """Untarring an unverified archive would scatter unverified files across
    the cache, so the sha256 is checked BEFORE tar runs."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    tarball = tmp_path / f"{run.name}.tar.gz"
    subprocess.run(["tar", "-czf", str(tarball), "-C", str(logs), run.name], check=True)
    archiver.push_tarball(tarball)

    key = cfg.tarballs_prefix() + run.name + ".tar.gz"
    fake.objects[key] = b"\x00" * len(fake.objects[key])  # same length, wrong bytes
    with pytest.raises(claude_worker.archive.ArchiveError, match="sha256"):
        archiver.pull_tarball(run.name, tmp_path / "cache3")
    assert not (tmp_path / "cache3" / run.name).exists()
    assert claude_worker.features.run_dirs(tmp_path / "cache3") == []


def test_verify_tarball_rejects_a_missing_object(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    archiver = make_archiver(cfg, fake)
    logs = tmp_path / "logs"
    run = make_run(logs, f"run-{EPOCH_NS}")
    tarball = tmp_path / f"{run.name}.tar.gz"
    subprocess.run(["tar", "-czf", str(tarball), "-C", str(logs), run.name], check=True)
    archiver.push_tarball(tarball)
    del fake.objects[cfg.tarballs_prefix() + run.name + ".tar.gz"]
    with pytest.raises(claude_worker.archive.ArchiveError, match="missing"):
        archiver.verify_tarball(run.name)


def test_key_order_property_lexicographic_equals_numeric(tmp_path: pathlib.Path) -> None:
    """`epoch_ns` is the partition key precisely so that a plain LIST comes back
    in chronological order. 19 digits until 2286, so no padding is needed — but
    prove it rather than assume it."""
    cfg = make_cfg(tmp_path)
    rng = random.Random(20260905)
    epochs = [
        rng.randrange(1_500_000_000_000_000_000, 9_999_999_999_999_999_999)
        for _ in range(10_000)
    ]
    keys = [cfg.index_key(f"run-{e}") for e in epochs]
    assert sorted(keys) == [cfg.index_key(f"run-{e}") for e in sorted(epochs)]


def test_run_id_accepts_a_path_or_an_id() -> None:
    assert claude_worker.archive._run_id("run-123") == "run-123"
    assert claude_worker.archive._run_id("/a/b/run-123") == "run-123"
    assert claude_worker.archive._run_id("/a/b/run-123/") == "run-123"
