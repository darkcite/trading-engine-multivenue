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
import shutil
import socket
import threading

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


def _stale_sock(path: pathlib.Path) -> None:
    """Leave a REFUSING socket inode at ``path`` — exactly what the
    engine's previous boot leaves behind (bind, close, no unlink)."""
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(str(path))
    s.close()  # inode stays; connect() now gets ECONNREFUSED


def test_stale_socket_retries_until_engine_binds(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """THE 2026-08-30→09-01 outage pin: a stale inode satisfies the
    existence wait instantly and the old code aborted on the first
    refused connect — four boots, VM inert. main must now RETRY
    within the budget and succeed once the (new) engine binds."""
    sock_dir = tests.conftest.short_sock_dir()
    sock = sock_dir / "ai.sock"
    _stale_sock(sock)
    for key, value in _cfg_env(tmp_path, sock).items():
        monkeypatch.setenv(key, value)
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    body = b'{"rows": []}'
    ruleset, report, full_hash = _write_bound_pair(tmp_path, body)
    _seed_committed(tmp_path / "state.db", ruleset, report, full_hash)

    server_box: list[tests.conftest.FakeUdsServer] = []

    def bind_late() -> None:
        sock.unlink()  # the new engine replaces the stale inode
        server = tests.conftest.FakeUdsServer(sock, tests.conftest.TEST_KEY)
        server.start()
        server_box.append(server)

    timer = threading.Timer(1.5, bind_late)
    timer.start()
    try:
        rc = claude_worker.recommit.main(["--wait-sock-seconds", "20"])
    finally:
        timer.join(timeout=10.0)
        for server in server_box:
            server.stop()
        shutil.rmtree(sock_dir, ignore_errors=True)
    assert rc == claude_worker.recommit.EXIT_OK
    assert server_box, "the late listener must have started"
    # heartbeat + stage + commit all landed on the late-bound engine.
    assert len(server_box[0].frames) >= 3


def test_stale_socket_exhausts_budget_as_transport(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """No engine ever binds: the retry loop must exhaust the budget
    (visibly retrying — never the old instant abort) and report
    transport."""
    sock_dir = tests.conftest.short_sock_dir()
    sock = sock_dir / "ai.sock"
    _stale_sock(sock)
    for key, value in _cfg_env(tmp_path, sock).items():
        monkeypatch.setenv(key, value)
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    body = b'{"rows": []}'
    ruleset, report, full_hash = _write_bound_pair(tmp_path, body)
    _seed_committed(tmp_path / "state.db", ruleset, report, full_hash)
    try:
        rc = claude_worker.recommit.main(["--wait-sock-seconds", "3"])
    finally:
        shutil.rmtree(sock_dir, ignore_errors=True)
    assert rc == claude_worker.recommit.EXIT_TRANSPORT
    err = capsys.readouterr().err
    assert "retrying" in err
    assert "budget exhausted" in err
