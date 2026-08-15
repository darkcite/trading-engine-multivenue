"""UDS client — the worker side of the §4 transport.

Design §4.3: ``uds.py`` is the **sole frame writer** in the worker process.
The worker is single-threaded by design (no asyncio, no writer threads), so
single-writer is a structural property; this class additionally enforces
the §5.4 protocol rule in code: **a Heartbeat precedes every payload frame
on each connection** — ``send_cmd`` refuses until ``send_heartbeat`` has
succeeded on the current connection (the engine's ai-exec strategy refuses
an intent that itself ends a silence window, so a worker that skipped the
heartbeat would lose exactly that intent).

Allocation discipline: ONE preallocated 82-B frame buffer per client
(§4.3), rewritten in place per frame; ``sendall`` hands the same buffer to
the kernel (the copy into the socket buffer is the kernel's, and the 64-B
ring memcpy on the engine side is the documented unavoidable copy).

Sequence numbers come from the durable allocator (``state.py``) so the
namespace survives reconnects; every successful send is recorded in the
event log with its wall-clock send time — the only structured send-time
record (§3 capture amendment).

Transport failures raise [`UdsError`]; the verb layer (item 12) maps them
to exit code 4.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import socket
import time

import claude_worker.frames
import claude_worker.state


class UdsError(Exception):
    """Transport-level failure: socket absent, busy, closed mid-send, or
    protocol misuse (payload before heartbeat)."""


class UdsClient:
    """Connect / heartbeat / send / close against the engine's UDS listener.

    Lifecycle per invocation (verbs) or per reconnect (serve)::

        client = claude_worker.uds.UdsClient(sock_path, key, state)
        client.connect()
        client.send_heartbeat(ts_ns)
        client.send_cmd(...)          # any number of payload frames
        client.close()
    """

    def __init__(
        self,
        sock_path: pathlib.Path,
        hmac_key: bytes,
        state: claude_worker.state.State,
    ) -> None:
        self._path: pathlib.Path = sock_path
        self._key: bytes = hmac_key
        self._state: claude_worker.state.State = state
        self._sock: socket.socket | None = None
        # The one per-connection frame buffer (§4.3).
        self._buf: bytearray = claude_worker.frames.new_frame_buffer()
        self._heartbeat_sent: bool = False

    def connect(self) -> None:
        """Connect to the engine socket. A refused/absent socket — engine
        down, or ``serve`` holding the single-client slot — raises
        [`UdsError`]."""
        if self._sock is not None:
            raise UdsError("already connected")
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(str(self._path))
        except OSError as exc:
            sock.close()
            raise UdsError(f"connect {self._path}: {exc}") from exc
        self._sock = sock
        self._heartbeat_sent = False

    def close(self) -> None:
        """Close the connection. Idempotent."""
        if self._sock is not None:
            self._sock.close()
            self._sock = None
        self._heartbeat_sent = False

    @property
    def connected(self) -> bool:
        """Whether a socket is currently open."""
        return self._sock is not None

    def send_heartbeat(self, ts_ns: int | None = None) -> int:
        """Send one Heartbeat frame (worker clock stamp — the engine
        rewrites it at accept, §13 decision 1). Returns the seq used."""
        return self._send(
            ts_ns=time.time_ns() if ts_ns is None else ts_ns,
            sym=claude_worker.frames.SYMBOL_ID_NONE,
            px=0,
            qty=0,
            ttl_ns=0,
            kind=claude_worker.frames.KIND_HEARTBEAT,
            venue=claude_worker.frames.VENUE_AI,
            strategy_id=claude_worker.frames.STRATEGY_SLOT_NONE,
            side=claude_worker.frames.SIDE_NONE,
            param_id=0,
            flags=0,
        )

    def send_cmd(  # noqa: PLR0913 — one keyword per §3 wire field, deliberately
        self,
        *,
        ts_ns: int | None = None,
        sym: int,
        px: int,
        qty: int,
        ttl_ns: int,
        kind: int,
        venue: int,
        strategy_id: int,
        side: int,
        param_id: int = 0,
        flags: int = 0,
    ) -> int:
        """Send one payload frame. Refuses (raises [`UdsError`]) unless a
        heartbeat has already gone out on THIS connection (§5.4). Returns
        the seq used."""
        if not self._heartbeat_sent:
            raise UdsError("protocol: heartbeat must precede payload frames (§5.4)")
        return self._send(
            ts_ns=time.time_ns() if ts_ns is None else ts_ns,
            sym=sym,
            px=px,
            qty=qty,
            ttl_ns=ttl_ns,
            kind=kind,
            venue=venue,
            strategy_id=strategy_id,
            side=side,
            param_id=param_id,
            flags=flags,
        )

    def _send(  # noqa: PLR0913 — one keyword per §3 wire field, deliberately
        self,
        *,
        ts_ns: int,
        sym: int,
        px: int,
        qty: int,
        ttl_ns: int,
        kind: int,
        venue: int,
        strategy_id: int,
        side: int,
        param_id: int,
        flags: int,
    ) -> int:
        sock = self._sock
        if sock is None:
            raise UdsError("not connected")
        seq = self._state.next_seq()
        claude_worker.frames.pack_frame(
            self._buf,
            self._key,
            ts_ns=ts_ns,
            seq=seq,
            sym=sym,
            px=px,
            qty=qty,
            ttl_ns=ttl_ns,
            kind=kind,
            venue=venue,
            strategy_id=strategy_id,
            side=side,
            param_id=param_id,
            flags=flags,
        )
        try:
            sock.sendall(self._buf)
        except OSError as exc:
            self.close()
            raise UdsError(f"send failed: {exc}") from exc
        send_ts = time.time_ns()
        # The only structured send-time record (§3 capture amendment).
        self._state.record_frame_sent(seq, kind, send_ts)
        if kind == claude_worker.frames.KIND_HEARTBEAT:
            self._heartbeat_sent = True
        return seq
