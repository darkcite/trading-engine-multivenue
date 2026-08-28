# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Ruling #7(b) post-boot re-commit module (2026-08-28 remediation plan).

Additive suite — the frozen 202 and the 8-verb surface are untouched.
The happy path runs the REAL stage/commit library functions against the
conftest FakeUdsServer (no mocking of the gate binding — the §6 law
must bind here exactly as in the verbs).

Convention: full ``import x`` only.
"""

import hashlib
import json
import pathlib

import pytest

import claude_worker.backtest
import claude_worker.config
import claude_worker.recommit
import claude_worker.state
import tests.conftest


def _cfg_env(tmp_path: pathlib.Path, sock: pathlib.Path) -> dict[str, str]:
    replay = tmp_path / "logs"
    replay.mkdir(exist_ok=True)
    return {
        "AI_INGRESS_SOCK": str(sock),
        "AI_INGRESS_HMAC_KEY": tests.conftest.TEST_KEY.hex(),
        "AI_RULESET_DIR": str(tmp_path / "rulesets"),
        "CLAUDE_WORKER_REPLAY_DIR": str(replay),
        "CLAUDE_WORKER_DB": str(tmp_path / "state.db"),
        "CLAUDE_WORKER_FEATURES_DIR": str(tmp_path / "features"),
        "CLAUDE_WORKER_MARKET_MAP": str(tmp_path / "market-map.json"),
        "RSS_FEEDS": "",
    }


def _write_bound_pair(tmp_path: pathlib.Path, body: bytes) -> tuple[pathlib.Path, pathlib.Path, str]:
    """A ruleset file + a schema-1 gates-passed report bound to it."""
    ruleset = tmp_path / "r.json"
    ruleset.write_bytes(body)
    full_hash = hashlib.sha256(body).hexdigest()
    report = tmp_path / "r.report.json"
    report.write_text(
        json.dumps(
            {
                "schema_version": claude_worker.backtest.REPORT_SCHEMA_VERSION,
                "ruleset_hash": full_hash,
                "gates": {"all_passed": True},
            }
        )
    )
    return ruleset, report, full_hash


def _seed_committed(db: pathlib.Path, ruleset: pathlib.Path, report: pathlib.Path, full_hash: str) -> None:
    state = claude_worker.state.State(db)
    try:
        state.stage_ruleset(full_hash, str(ruleset), str(report), "session")
        state.mark_ruleset_committed(full_hash)
    finally:
        state.close()


def test_no_committed_row_is_honest_noop(tmp_path: pathlib.Path) -> None:
    # Failure-mode-of-absence: fresh install — no registry rows, no
    # socket ever touched (the sock path does not even exist).
    cfg = claude_worker.config.load_base_from_env(
        _cfg_env(tmp_path, tmp_path / "absent.sock")
    )
    lines: list[str] = []
    rc = claude_worker.recommit.recommit_active(cfg, report=lines.append)
    assert rc == claude_worker.recommit.EXIT_OK
    assert any("honest no-op" in line for line in lines)


def test_recommit_restages_and_recommits_active_row(
    tmp_path: pathlib.Path, fake_uds: tests.conftest.FakeUdsServer
) -> None:
    # Happy path: committed registry row + intact bound files ⇒ the
    # REAL gate binding re-verifies, the REAL frames go out (heartbeat
    # + stage + commit against the conftest fake engine listener), and
    # the registry row is committed again.
    cfg = claude_worker.config.load_base_from_env(_cfg_env(tmp_path, fake_uds.sock_path))
    ruleset, report, full_hash = _write_bound_pair(tmp_path, b'{"rows":[1]}')
    _seed_committed(cfg.db_path, ruleset, report, full_hash)

    lines: list[str] = []
    rc = claude_worker.recommit.recommit_active(cfg, report=lines.append)
    assert rc == claude_worker.recommit.EXIT_OK
    assert any(full_hash in line for line in lines)
    # heartbeat + RulesetStage + RulesetCommit all reached the engine.
    assert len(fake_uds.frames) == 3
    assert fake_uds.errors == []
    state = claude_worker.state.State(cfg.db_path)
    try:
        row = state.ruleset_row(full_hash)
    finally:
        state.close()
    assert row is not None
    assert row[6] is not None  # committed_ts stamped again


def test_drifted_bound_file_refuses_before_commit(
    tmp_path: pathlib.Path, fake_uds: tests.conftest.FakeUdsServer
) -> None:
    # Failure mode: the file under the bound path no longer holds the
    # committed bytes. Re-stage succeeds (new hash, valid new report)
    # but the module must refuse to commit the OLD hash against it.
    cfg = claude_worker.config.load_base_from_env(_cfg_env(tmp_path, fake_uds.sock_path))
    ruleset, report, old_hash = _write_bound_pair(tmp_path, b'{"rows":[1]}')
    _seed_committed(cfg.db_path, ruleset, report, old_hash)
    # Drift the bound pair to different bytes.
    _write_bound_pair(tmp_path, b'{"rows":[2]}')

    lines: list[str] = []
    rc = claude_worker.recommit.recommit_active(cfg, report=lines.append)
    assert rc == claude_worker.recommit.EXIT_GATE
    assert any("drifted" in line for line in lines)
    # Heartbeat + the stage frame went out; NO commit frame followed.
    assert len(fake_uds.frames) == 2
    state = claude_worker.state.State(cfg.db_path)
    try:
        committed = state.committed_rulesets()
    finally:
        state.close()
    # The old row is still the only committed one (drift never commits).
    assert [row[0] for row in committed] == [old_hash]


def test_missing_bound_paths_refuse(tmp_path: pathlib.Path) -> None:
    # Failure mode: registry points at deleted files.
    cfg = claude_worker.config.load_base_from_env(
        _cfg_env(tmp_path, tmp_path / "absent.sock")
    )
    ruleset, report, full_hash = _write_bound_pair(tmp_path, b'{"rows":[3]}')
    _seed_committed(cfg.db_path, ruleset, report, full_hash)
    ruleset.unlink()
    lines: list[str] = []
    rc = claude_worker.recommit.recommit_active(cfg, report=lines.append)
    assert rc == claude_worker.recommit.EXIT_GATE
    assert any("bound paths missing" in line for line in lines)


def test_wait_for_sock_times_out_fast(tmp_path: pathlib.Path) -> None:
    assert claude_worker.recommit.wait_for_sock(tmp_path / "never.sock", 0.0) is False


def test_main_reports_transport_when_sock_never_appears(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    for key, value in _cfg_env(tmp_path, tmp_path / "never.sock").items():
        monkeypatch.setenv(key, value)
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    rc = claude_worker.recommit.main(["--wait-sock-seconds", "0"])
    assert rc == claude_worker.recommit.EXIT_TRANSPORT
