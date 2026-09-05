# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Gate G-S6 — agent awareness, added without changing anything that exists.

The whole point of S6 is that it is invisible when off. The nightly per-regime
P&L report is what the RG7 soak reads, so the load-bearing test here is
`test_disabled_report_has_no_data_source_key`: with no archive configured, a
report must be byte-identical to one built before this phase existed.

Convention: full ``import x`` only. No ``from x import y``.
"""

import datetime
import json
import pathlib
import shutil
import struct
import typing

import pytest

import claude_worker.archive
import claude_worker.archive_config
import claude_worker.cli
import claude_worker.data_source
import claude_worker.objstore
import claude_worker.pnl_report
import tests.fake_s3

HEADER = struct.Struct("<4sHBxQ")

ENV: typing.Final[dict[str, str]] = {
    "MULTIVENUE_S3_ENABLED": "1",
    "MULTIVENUE_S3_BUCKET": "market-data-qu",
    "MULTIVENUE_S3_ENDPOINT": "fsn1.your-objectstorage.com",
    "MULTIVENUE_S3_REGION": "fsn1",
    "MULTIVENUE_S3_HOST_ID": "mbp-m4",
    "MULTIVENUE_S3_ACCESS_KEY_ID": "AK",
    "MULTIVENUE_S3_SECRET_ACCESS_KEY": "sekrit",
    "MULTIVENUE_S3_CACHE_MIN_FREE_GIB": "0",
}


def epoch_for(day: str, hour: int = 3) -> int:
    stamp = datetime.datetime.strptime(day, "%Y-%m-%d").replace(
        hour=hour, tzinfo=datetime.timezone.utc
    )
    return int(stamp.timestamp() * 1e9)


def make_run(root: pathlib.Path, epoch_ns: int) -> pathlib.Path:
    run = root / f"run-{epoch_ns}"
    run.mkdir(parents=True)
    body = bytearray(HEADER.pack(b"PMLR", 3, 0, epoch_ns))
    body.extend(b"\x00" * (64 - len(body)))
    for i in range(20):
        slot = bytearray(64)
        slot[0:8] = struct.pack("<Q", epoch_ns + i * 1_000_000_000)
        body.extend(slot)
    (run / "pm-ticks.pmlr").write_bytes(bytes(body))
    return run


def make_cfg(
    tmp_path: pathlib.Path, **overrides: str
) -> claude_worker.archive_config.ArchiveConfig:
    env = dict(ENV)
    env["MULTIVENUE_S3_CACHE_DIR"] = str(tmp_path / "cache")
    env.update(overrides)
    return claude_worker.archive_config.load(env=env, env_file=tmp_path / "absent.env")


def wire(
    cfg: claude_worker.archive_config.ArchiveConfig, fake: tests.fake_s3.FakeS3
) -> claude_worker.objstore.ObjectStore:
    assert cfg.endpoint is not None and cfg.creds is not None
    return claude_worker.objstore.ObjectStore(
        cfg.endpoint, cfg.creds, fake.client(), timeout_s=5.0, sleep=lambda _s: None
    )


# --------------------------------------------------------------------------
# select_runs
# --------------------------------------------------------------------------


def test_select_runs_is_unchanged_without_a_source(tmp_path: pathlib.Path) -> None:
    logs = tmp_path / "logs"
    day = "2026-09-01"
    wanted = make_run(logs, epoch_for(day))
    make_run(logs, epoch_for("2026-09-02"))
    assert claude_worker.pnl_report.select_runs(logs, day) == [wanted]


def test_select_runs_materialises_an_archived_day(tmp_path: pathlib.Path) -> None:
    """The case that used to be impossible: re-running a day retention has
    already reclaimed."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    logs = tmp_path / "logs"
    day = "2026-09-01"
    archived = make_run(logs, epoch_for(day, hour=3))
    make_run(logs, epoch_for(day, hour=5))
    make_run(logs, epoch_for("2026-09-02"))  # newest, never pushed

    claude_worker.archive.Archiver(cfg, wire(cfg, fake)).push_pending(logs)
    # Retention reclaims the first run of that day.
    shutil.rmtree(archived)
    assert claude_worker.pnl_report.select_runs(logs, day) == [
        logs / f"run-{epoch_for(day, hour=5)}"
    ]

    source = claude_worker.data_source.DataSource(cfg, logs, wire(cfg, fake))
    resolved = claude_worker.pnl_report.select_runs(logs, day, source)
    assert [p.name for p in resolved] == [
        f"run-{epoch_for(day, hour=3)}",
        f"run-{epoch_for(day, hour=5)}",
    ]
    assert resolved[0] == cfg.cache_dir / f"run-{epoch_for(day, hour=3)}"
    assert (resolved[0] / "pm-ticks.pmlr").is_file()


def test_select_runs_keeps_local_runs_when_a_pull_fails(tmp_path: pathlib.Path) -> None:
    """A broken archive must degrade to today's behaviour, not to an empty day."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    logs = tmp_path / "logs"
    day = "2026-09-01"
    archived = make_run(logs, epoch_for(day, hour=3))
    local = make_run(logs, epoch_for(day, hour=5))
    make_run(logs, epoch_for("2026-09-02"))
    claude_worker.archive.Archiver(cfg, wire(cfg, fake)).push_pending(logs)
    shutil.rmtree(archived)
    # Break the archived copy after the index was written.
    manifest = json.loads(fake.objects[cfg.manifest_key(archived.name)])
    del fake.objects[manifest["files"][0]["key"]]

    source = claude_worker.data_source.DataSource(cfg, logs, wire(cfg, fake))
    assert claude_worker.pnl_report.select_runs(logs, day, source) == [local]


# --------------------------------------------------------------------------
# the report block
# --------------------------------------------------------------------------


def test_disabled_report_has_no_data_source_key(tmp_path: pathlib.Path) -> None:
    """S-LAW 2 for the nightly report the RG7 soak reads."""
    logs = tmp_path / "logs"
    make_run(logs, epoch_for("2026-09-01"))
    reports = tmp_path / "reports"
    lines: list[str] = []

    def fake_run(argv: list[str]) -> tuple[int, str, str]:
        return 0, json.dumps({"audit_pnl_version": 1}), ""

    claude_worker.pnl_report.run_day(
        logs, reports, "2026-09-01", lines.append, run_fn=fake_run
    )
    body = json.loads((reports / "pnl-2026-09-01.json").read_text(encoding="utf-8"))
    assert "data_source" not in body
    assert not any("archive:" in line for line in lines)


def test_report_data_source_block_matches_where(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    logs = tmp_path / "logs"
    day = "2026-09-01"
    make_run(logs, epoch_for(day))
    make_run(logs, epoch_for("2026-09-02"))
    claude_worker.archive.Archiver(cfg, wire(cfg, fake)).push_pending(logs)
    source = claude_worker.data_source.DataSource(cfg, logs, wire(cfg, fake))

    reports = tmp_path / "reports"
    lines: list[str] = []

    def fake_run(argv: list[str]) -> tuple[int, str, str]:
        return 0, json.dumps({"audit_pnl_version": 1}), ""

    claude_worker.pnl_report.run_day(
        logs, reports, day, lines.append, run_fn=fake_run, source=source
    )
    body = json.loads((reports / f"pnl-{day}.json").read_text(encoding="utf-8"))
    assert body["data_source"]["archive_enabled"] is True
    row = body["data_source"]["runs"][0]
    assert row["run"] == f"run-{epoch_for(day)}"
    assert row["location"] == source.where(row["run"]) == "local+s3"
    assert any(line.startswith("archive: enabled") for line in lines)


def test_report_still_carries_every_pre_existing_key(tmp_path: pathlib.Path) -> None:
    """The additive rule, checked against the documented top-level shape."""
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    logs = tmp_path / "logs"
    day = "2026-09-01"
    make_run(logs, epoch_for(day))
    make_run(logs, epoch_for("2026-09-02"))
    source = claude_worker.data_source.DataSource(cfg, logs, wire(cfg, fake))
    reports = tmp_path / "reports"

    def fake_run(argv: list[str]) -> tuple[int, str, str]:
        return 0, json.dumps({"audit_pnl_version": 1}), ""

    claude_worker.pnl_report.run_day(
        logs, reports, day, lambda _s: None, run_fn=fake_run, source=source
    )
    body = json.loads((reports / f"pnl-{day}.json").read_text(encoding="utf-8"))
    for key in (
        "audit_pnl_version",
        "day",
        "runs",
        "window",
        "paper",
        "strategies",
        "vm_by_ruleset",
        "regime",
        "runs_detail",
        "failed_runs",
        "fee_flags",
    ):
        assert key in body, key
    assert body["audit_pnl_version"] == 1


def test_list_row_shape_is_what_the_agent_reads(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    fake = tests.fake_s3.FakeS3()
    logs = tmp_path / "logs"
    make_run(logs, epoch_for("2026-09-01"))
    make_run(logs, epoch_for("2026-09-02"))
    archiver = claude_worker.archive.Archiver(cfg, wire(cfg, fake))
    archiver.push_pending(logs)
    row = claude_worker.archive._list_row(archiver.list_runs()[0])
    assert set(row) == {
        "run",
        "epoch_ns",
        "host_id",
        "location",
        "span_ns",
        "size_bytes",
        "stored_bytes",
        "pmlr_version",
        "windows_2h_complete",
        "venues",
        "local_path",
        "uploaded_at_ns",
    }
    # windows_2h_complete is what lets the agent plan an N>=4 disjoint-window
    # pool BEFORE deciding what to pull.
    assert row["windows_2h_complete"] == 0
    json.dumps(row)  # must be serialisable as one NDJSON line


def test_status_tell_is_house_style(tmp_path: pathlib.Path) -> None:
    cfg = make_cfg(tmp_path)
    assert cfg.tell() == "archive: enabled bucket=market-data-qu host=mbp-m4"
    off = claude_worker.archive_config.load(env={}, env_file=tmp_path / "absent.env")
    assert off.tell() == "archive: disabled (flag)"


def test_resolve_run_dir_still_rejects_a_bad_path_when_disabled(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """With no archive, the error is exactly the one this verb always gave."""
    disabled = claude_worker.archive_config.load(env={}, env_file=tmp_path / "absent.env")
    monkeypatch.setattr(claude_worker.cli.claude_worker.archive_config, "load", lambda: disabled)

    class Cfg:
        replay_dir = tmp_path / "logs"

    # ValueError is this CLI's usage error (mapped to exit 2), and the message
    # is unchanged from before the archive existed.
    with pytest.raises(ValueError, match="no such directory"):
        claude_worker.cli._resolve_run_dir(
            typing.cast(typing.Any, Cfg()), "run-1788417289611943000"
        )
