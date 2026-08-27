# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""uds.py — client behavior against the fake UDS server (§11).

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import shutil
import time

import pytest

import claude_worker.frames
import claude_worker.state
import claude_worker.uds
import tests.conftest


def _wait_for_frames(server: tests.conftest.FakeUdsServer, count: int) -> None:
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if len(server.frames) >= count:
            return
        time.sleep(0.01)
    raise AssertionError(
        f"fake server saw {len(server.frames)} frames, wanted {count}; errors={server.errors}"
    )


@pytest.fixture
def state(tmp_path: pathlib.Path) -> claude_worker.state.State:
    return claude_worker.state.State(tmp_path / "state.db")


def _client(
    server: tests.conftest.FakeUdsServer, st: claude_worker.state.State
) -> claude_worker.uds.UdsClient:
    return claude_worker.uds.UdsClient(server.sock_path, tests.conftest.TEST_KEY, st)


def test_heartbeat_then_payload_reaches_server_verified(
    fake_uds: tests.conftest.FakeUdsServer, state: claude_worker.state.State
) -> None:
    client = _client(fake_uds, state)
    client.connect()
    hb_seq = client.send_heartbeat(ts_ns=1_000)
    cmd_seq = client.send_cmd(
        ts_ns=2_000,
        sym=7,
        px=500_000,
        qty=0,
        ttl_ns=60_000_000_000,
        kind=claude_worker.frames.KIND_SET_FAIR_VALUE,
        venue=claude_worker.frames.VENUE_AI,
        strategy_id=claude_worker.frames.STRATEGY_SLOT_NONE,
        side=claude_worker.frames.SIDE_NONE,
    )
    client.close()

    _wait_for_frames(fake_uds, 2)
    assert fake_uds.errors == [], "every frame must pass len + HMAC checks"
    # §5.4: the heartbeat precedes the payload on the wire.
    assert fake_uds.cmd_field(0, "kind") == claude_worker.frames.KIND_HEARTBEAT
    assert fake_uds.cmd_field(1, "kind") == claude_worker.frames.KIND_SET_FAIR_VALUE
    assert fake_uds.cmd_field(0, "seq") == hb_seq
    assert fake_uds.cmd_field(1, "seq") == cmd_seq
    assert cmd_seq == hb_seq + 1
    assert fake_uds.cmd_field(1, "sym") == 7
    assert fake_uds.cmd_field(1, "px") == 500_000
    assert fake_uds.cmd_field(1, "ttl_ns") == 60_000_000_000


def test_payload_before_heartbeat_refused_in_code(
    fake_uds: tests.conftest.FakeUdsServer, state: claude_worker.state.State
) -> None:
    client = _client(fake_uds, state)
    client.connect()
    with pytest.raises(claude_worker.uds.UdsError, match="heartbeat"):
        client.send_cmd(
            sym=7,
            px=500_000,
            qty=0,
            ttl_ns=60_000_000_000,
            kind=claude_worker.frames.KIND_SET_FAIR_VALUE,
            venue=claude_worker.frames.VENUE_AI,
            strategy_id=claude_worker.frames.STRATEGY_SLOT_NONE,
            side=claude_worker.frames.SIDE_NONE,
        )
    client.close()
    assert fake_uds.frames == [], "nothing may reach the wire"


def test_reconnect_requires_fresh_heartbeat(
    fake_uds: tests.conftest.FakeUdsServer, state: claude_worker.state.State
) -> None:
    client = _client(fake_uds, state)
    client.connect()
    client.send_heartbeat(ts_ns=1)
    client.close()
    client.connect()
    # Old connection's heartbeat must not carry over (§5.4 is
    # per-connection).
    with pytest.raises(claude_worker.uds.UdsError, match="heartbeat"):
        client.send_cmd(
            sym=claude_worker.frames.SYMBOL_ID_NONE,
            px=0,
            qty=0,
            ttl_ns=0,
            kind=claude_worker.frames.KIND_HALT_REQUEST,
            venue=claude_worker.frames.VENUE_AI,
            strategy_id=claude_worker.frames.STRATEGY_SLOT_NONE,
            side=claude_worker.frames.SIDE_NONE,
        )
    client.send_heartbeat(ts_ns=2)
    client.close()
    _wait_for_frames(fake_uds, 2)
    assert fake_uds.errors == []


def test_seq_survives_reconnect_and_is_strictly_increasing(
    fake_uds: tests.conftest.FakeUdsServer, state: claude_worker.state.State
) -> None:
    client = _client(fake_uds, state)
    client.connect()
    s1 = client.send_heartbeat(ts_ns=1)
    client.close()
    client.connect()
    s2 = client.send_heartbeat(ts_ns=2)
    client.close()
    assert s2 == s1 + 1, "durable allocator spans connections"
    _wait_for_frames(fake_uds, 2)
    assert fake_uds.cmd_field(0, "seq") == s1
    assert fake_uds.cmd_field(1, "seq") == s2


def test_send_time_recorded_in_event_log(
    fake_uds: tests.conftest.FakeUdsServer, state: claude_worker.state.State
) -> None:
    before = time.time_ns()
    client = _client(fake_uds, state)
    client.connect()
    hb_seq = client.send_heartbeat()
    client.close()
    after = time.time_ns()

    events = state.events(kind=claude_worker.state.EVENT_FRAME_SENT)
    assert len(events) == 1
    _id, ts, _kind, detail = events[0]
    # §3 capture amendment: this row is the only structured send-time
    # record — the stamp must be a real wall-clock send time.
    assert before <= ts <= after
    assert f"seq={hb_seq}" in detail
    assert f"kind={claude_worker.frames.KIND_HEARTBEAT}" in detail


def test_connect_to_absent_socket_raises(state: claude_worker.state.State) -> None:
    # Short path so the failure is ENOENT, not macOS sun_path overflow.
    sock_dir = tests.conftest.short_sock_dir()
    try:
        client = claude_worker.uds.UdsClient(
            sock_dir / "absent.sock", tests.conftest.TEST_KEY, state
        )
        with pytest.raises(claude_worker.uds.UdsError, match="connect"):
            client.connect()
        assert not client.connected
    finally:
        shutil.rmtree(sock_dir, ignore_errors=True)


def test_double_connect_refused(
    fake_uds: tests.conftest.FakeUdsServer, state: claude_worker.state.State
) -> None:
    client = _client(fake_uds, state)
    client.connect()
    with pytest.raises(claude_worker.uds.UdsError, match="already connected"):
        client.connect()
    client.close()
