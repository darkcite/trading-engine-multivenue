# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Shared fixtures (design §11): the fake UDS server.

Accepts clients sequentially on a private per-test socket path, reads
82-byte frames, length-checks and HMAC-verifies each with the test key,
and records the raw command bytes in arrival order — the assertion
surface for commander/verb/heartbeat tests from item 9 onward.

Test hygiene: sockets live under a short private ``/tmp/cw-ai-<pid>-*/``
dir (macOS ``sun_path`` cap — see [`short_sock_dir`]; never the live
``AI_INGRESS_SOCK``); no live engine, no live venues, no Anthropic SDK
anywhere.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import hashlib
import hmac
import os
import pathlib
import shutil
import socket
import tempfile
import threading
import types
import typing

import anthropic.types
import pytest

import claude_worker.frames

# Fixture-only key — also the key inside tests/fixtures/ai_frame_golden.txt.
TEST_KEY: bytes = bytes(range(32))


class FakeUdsServer:
    """Minimal stand-in for the engine's ``ingress-ai`` listener.

    Records every well-formed frame's 64 command bytes in ``frames`` and
    every violation in ``errors``. Verification order mirrors §4.4:
    length field first, then the truncated-16 HMAC tag.
    """

    def __init__(self, sock_path: pathlib.Path, key: bytes) -> None:
        self.sock_path: pathlib.Path = sock_path
        self._key: bytes = key
        self.frames: list[bytes] = []
        self.errors: list[str] = []
        self._listener: socket.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()

    def start(self) -> None:
        self.sock_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        self._listener.bind(str(self.sock_path))
        self.sock_path.chmod(0o600)
        self._listener.listen(1)
        # Accept loop wakes regularly to observe the stop flag.
        self._listener.settimeout(0.05)
        self._thread = threading.Thread(target=self._run, name="fake-uds", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        thread = self._thread
        if thread is not None:
            thread.join(timeout=5.0)
            if thread.is_alive():  # pragma: no cover - diagnostic guard
                raise RuntimeError("fake UDS server thread failed to stop")
        self._listener.close()

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _addr = self._listener.accept()
            except TimeoutError:
                continue
            except OSError:  # pragma: no cover - listener closed under us
                return
            with conn:
                conn.settimeout(0.05)
                self._serve_one(conn)

    def _serve_one(self, conn: socket.socket) -> None:
        frame_len = claude_worker.frames.FRAME_LEN
        buf = bytearray()
        while not self._stop.is_set():
            try:
                chunk = conn.recv(4096)
            except TimeoutError:
                continue
            except OSError:
                return
            if not chunk:
                return  # peer closed
            buf.extend(chunk)
            while len(buf) >= frame_len:
                frame = bytes(buf[:frame_len])
                del buf[:frame_len]
                self._check(frame)

    def _check(self, frame: bytes) -> None:
        lf = int.from_bytes(frame[0:2], "little")
        if lf != claude_worker.frames.LEN_FIELD_VALUE:
            self.errors.append(f"bad len field: {lf}")
            return
        cmd = frame[claude_worker.frames.CMD_OFFSET : claude_worker.frames.TAG_OFFSET]
        tag = frame[claude_worker.frames.TAG_OFFSET :]
        want = hmac.new(self._key, cmd, hashlib.sha256).digest()[: claude_worker.frames.TAG_LEN]
        if not hmac.compare_digest(tag, want):
            self.errors.append("hmac mismatch")
            return
        self.frames.append(cmd)

    def cmd_field(self, index: int, name: str) -> int:
        """Decode one field of recorded frame ``index`` (test sugar)."""
        cmd = self.frames[index]
        offsets = {
            "ts_ns": (0, 8, False),
            "seq": (8, 4, False),
            "sym": (12, 4, False),
            "px": (16, 8, True),
            "qty": (24, 8, True),
            "ttl_ns": (32, 8, False),
            "kind": (40, 1, False),
            "venue": (41, 1, False),
            "strategy_id": (42, 1, False),
            "side": (43, 1, False),
            "param_id": (44, 2, False),
            "flags": (46, 2, False),
        }
        off, size, signed = offsets[name]
        return int.from_bytes(cmd[off : off + size], "little", signed=signed)


class _FakeMessages:
    """Programmed ``messages.create`` double (§9.1 carried pattern)."""

    def __init__(self, respond: typing.Callable[[str, str], str]) -> None:
        self._respond = respond
        self.calls: list[tuple[str, str]] = []

    def create(
        self,
        *,
        model: str,
        max_tokens: int,
        messages: list[dict[str, str]],
    ) -> types.SimpleNamespace:
        del max_tokens
        prompt = messages[0]["content"]
        self.calls.append((model, prompt))
        block = anthropic.types.TextBlock(
            type="text", text=self._respond(model, prompt), citations=None
        )
        return types.SimpleNamespace(content=[block])


class FakeClient:
    """SDK-shaped double for the ``llm.py`` seam: tests monkeypatch
    ``claude_worker.llm.make_client`` to return one of these. Real
    ``anthropic.types.TextBlock`` instances keep ``llm.complete``'s
    isinstance narrowing honest; no network anywhere."""

    def __init__(self, respond: typing.Callable[[str, str], str]) -> None:
        self.messages = _FakeMessages(respond)

    @property
    def calls(self) -> list[tuple[str, str]]:
        """(model, prompt) pairs, in call order."""
        return self.messages.calls


def short_sock_dir() -> pathlib.Path:
    """A short private dir for UDS sockets.

    macOS caps ``sun_path`` at ~104 bytes; pytest's ``tmp_path`` under
    ``/private/var/folders/…`` blows past it (S4 landmine). ``mkdtemp``
    under ``/tmp`` is short and 0700, and the ``cw-ai-<pid>-`` prefix
    satisfies the session hygiene rule (own parent dir, never the live
    socket).
    """
    return pathlib.Path(tempfile.mkdtemp(prefix=f"cw-ai-{os.getpid()}-", dir="/tmp"))


@pytest.fixture
def fake_uds() -> collections.abc.Iterator[FakeUdsServer]:
    """A running FakeUdsServer on a private short-path socket; stopped and
    cleaned up at teardown."""
    sock_dir = short_sock_dir()
    server = FakeUdsServer(sock_dir / "ai.sock", TEST_KEY)
    server.start()
    yield server
    server.stop()
    shutil.rmtree(sock_dir, ignore_errors=True)
