"""commander.py against the fake UDS server (§11): policy gate, SetBias
frame shape per the §3 table, TTL clamps, 5 s heartbeat cadence, and the
transport protocol rule staying enforced underneath.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import pathlib
import time

import pytest

import claude_worker.commander
import claude_worker.frames
import claude_worker.labeling
import claude_worker.state
import claude_worker.uds
import tests.conftest

_Wired = tuple[tests.conftest.FakeUdsServer, claude_worker.uds.UdsClient, claude_worker.state.State]


def _wait_for_frames(server: tests.conftest.FakeUdsServer, count: int) -> None:
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if len(server.frames) >= count:
            return
        time.sleep(0.01)
    raise AssertionError(f"expected {count} frames, got {len(server.frames)}")


@pytest.fixture
def wired(
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
) -> collections.abc.Iterator[_Wired]:
    state = claude_worker.state.State(tmp_path / "state.db")
    client = claude_worker.uds.UdsClient(fake_uds.sock_path, tests.conftest.TEST_KEY, state)
    client.connect()
    yield fake_uds, client, state
    client.close()
    state.close()


def _label(
    confidence: float = 0.9, direction: str = "up", half_life_s: float = 600.0
) -> claude_worker.labeling.Label:
    return claude_worker.labeling.Label(
        sym=7, direction=direction, confidence=confidence, half_life_s=half_life_s
    )


def test_heartbeat_cadence(wired: _Wired) -> None:
    server, client, _state = wired
    commander = claude_worker.commander.Commander(client, claude_worker.commander.Policy())
    assert commander.maybe_heartbeat(0) is True  # first call always due
    assert commander.maybe_heartbeat(4_999_999_999) is False
    assert commander.maybe_heartbeat(5_000_000_000) is True
    _wait_for_frames(server, 2)
    assert server.cmd_field(0, "kind") == claude_worker.frames.KIND_HEARTBEAT
    assert server.cmd_field(1, "kind") == claude_worker.frames.KIND_HEARTBEAT
    assert server.errors == []


def test_reset_cadence_on_reconnect(wired: _Wired) -> None:
    server, client, _state = wired
    commander = claude_worker.commander.Commander(client, claude_worker.commander.Policy())
    assert commander.maybe_heartbeat(0) is True
    commander.reset_cadence()
    assert commander.maybe_heartbeat(1) is True  # immediately due again
    _wait_for_frames(server, 2)


def test_emit_set_bias_frame_shape(wired: _Wired) -> None:
    server, client, _state = wired
    commander = claude_worker.commander.Commander(client, claude_worker.commander.Policy())
    commander.maybe_heartbeat(0)
    seq = commander.emit(_label(confidence=0.9, direction="up", half_life_s=600.0))
    assert seq is not None
    assert commander.emitted_total == 1
    _wait_for_frames(server, 2)
    # §3 SetBias row: sym, signed px, qty 0, ttl required, 0xFF slots.
    assert server.cmd_field(1, "kind") == claude_worker.frames.KIND_SET_BIAS
    assert server.cmd_field(1, "sym") == 7
    assert server.cmd_field(1, "px") == 18_000  # 20_000 * 0.9
    assert server.cmd_field(1, "qty") == 0
    assert server.cmd_field(1, "ttl_ns") == 600_000_000_000
    assert server.cmd_field(1, "venue") == claude_worker.frames.VENUE_AI
    assert server.cmd_field(1, "strategy_id") == claude_worker.frames.STRATEGY_SLOT_NONE
    assert server.cmd_field(1, "side") == claude_worker.frames.SIDE_NONE
    assert server.cmd_field(1, "param_id") == 0
    assert server.cmd_field(1, "flags") == claude_worker.frames.FLAG_EXPIRE_ON_SILENCE
    assert server.cmd_field(1, "seq") == seq
    assert server.errors == []


def test_emit_down_direction_negative_bias(wired: _Wired) -> None:
    server, client, _state = wired
    commander = claude_worker.commander.Commander(client, claude_worker.commander.Policy())
    commander.maybe_heartbeat(0)
    commander.emit(_label(confidence=1.0, direction="down"))
    _wait_for_frames(server, 2)
    assert server.cmd_field(1, "px") == -20_000


def test_emit_without_expire_on_silence(wired: _Wired) -> None:
    server, client, _state = wired
    policy = claude_worker.commander.Policy(expire_on_silence=False)
    commander = claude_worker.commander.Commander(client, policy)
    commander.maybe_heartbeat(0)
    commander.emit(_label())
    _wait_for_frames(server, 2)
    assert server.cmd_field(1, "flags") == 0


def test_low_confidence_refused(wired: _Wired) -> None:
    server, client, _state = wired
    commander = claude_worker.commander.Commander(client, claude_worker.commander.Policy())
    commander.maybe_heartbeat(0)
    assert commander.emit(_label(confidence=0.5)) is None
    assert commander.refused_low_confidence_total == 1
    assert commander.emitted_total == 0
    _wait_for_frames(server, 1)  # heartbeat only, no payload
    time.sleep(0.05)  # grace: a stray payload frame would still be in flight
    assert len(server.frames) == 1


def test_ttl_clamps() -> None:
    policy = claude_worker.commander.Policy()
    assert policy.ttl_ns_for(0.2) == 1_000_000_000  # floor 1 s
    assert policy.ttl_ns_for(600.0) == 600_000_000_000
    assert policy.ttl_ns_for(1e9) == 3_600_000_000_000  # cap 1 h


def test_payload_before_heartbeat_still_refused(wired: _Wired) -> None:
    # The item-9 transport rule is not weakened by the commander layer.
    _server, client, _state = wired
    commander = claude_worker.commander.Commander(client, claude_worker.commander.Policy())
    with pytest.raises(claude_worker.uds.UdsError, match="heartbeat"):
        commander.emit(_label())
