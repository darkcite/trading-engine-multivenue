"""``serve`` composition loop (design §5.2 FULL-AUTO — the only daemon
mode).

Single-threaded cooperative loop, cadences off an injected monotonic
clock, no asyncio (§5.2 carried decision). Composition per iteration:

1. UDS connect/reconnect (engine may be down or a verb may briefly hold
   the single-client slot — both are counted retries, never fatal);
   a fresh connection resets the commander cadence so the item-9
   heartbeat-precedes-payload rule is satisfied immediately.
2. Commander heartbeat (5 s §13-d6 cadence).
3. news_watcher poll (its own per-feed 15-60 s jittered cadences) —
   triage/label through the prompt cache with the ``llm.py`` client.
4. Labels -> ``Commander.emit``. Labels produced while the engine is
   unreachable are DROPPED and counted (documented call: news intel is
   TTL'd by design — queueing stale pressure for a returning engine
   would be worse than losing it; §5.4 fail-safe reasoning).

``llm.make_client`` is invoked HERE and nowhere else (§5.2: the only
place ``ANTHROPIC_API_KEY`` is read is ServeConfig; tests monkeypatch
the seam). SIGTERM/SIGINT flip a stop flag; shutdown flushes SQLite
(close), closes the UDS, restores prior signal handlers, returns 0.

The strategist/backtest cadence (6 h proposal loop) is deliberately NOT
composed yet: ``strategist.py`` has no checklist item in §12 — flagged
to the operator in the S5 progress entry; the seam will slot in beside
the watcher when scoped (no dead code until then).

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import random
import signal
import time
import types
import typing

import httpx
import structlog

import claude_worker.commander
import claude_worker.config
import claude_worker.feeds
import claude_worker.labeling
import claude_worker.llm
import claude_worker.state
import claude_worker.uds

# Cooperative loop tick: bounds shutdown latency and cadence resolution.
TICK_S: float = 0.2

_log = structlog.get_logger("claude_worker.daemon")


@dataclasses.dataclass(slots=True)
class ServeStats:
    """Loop counters (test/diagnostic surface; mutated in place when the
    caller passes one in)."""

    iterations: int = 0
    heartbeats: int = 0
    labels_emitted: int = 0
    labels_refused: int = 0
    labels_dropped_disconnected: int = 0
    uds_errors: int = 0
    connect_failures: int = 0


class _StopFlag:
    """Signal-flipped stop flag; handlers installed/restored by serve()."""

    def __init__(self) -> None:
        self.stop: bool = False

    def trip(self, _signum: int, _frame: types.FrameType | None) -> None:
        self.stop = True


def _ensure_connected(
    client: claude_worker.uds.UdsClient,
    commander: claude_worker.commander.Commander,
    stats: ServeStats,
) -> bool:
    if client.connected:
        return True
    try:
        client.connect()
    except claude_worker.uds.UdsError:
        stats.connect_failures += 1
        return False
    commander.reset_cadence()
    return True


def _emit_labels(
    commander: claude_worker.commander.Commander,
    uds_client: claude_worker.uds.UdsClient,
    labels: list[claude_worker.labeling.Label],
    connected: bool,
    stats: ServeStats,
) -> None:
    """Hand one poll cycle's labels to the commander; disconnected or
    mid-send-failed labels are dropped and counted (module doc §4)."""
    for label in labels:
        if not connected:
            stats.labels_dropped_disconnected += 1
            continue
        try:
            seq = commander.emit(label)
        except claude_worker.uds.UdsError:
            stats.uds_errors += 1
            uds_client.close()
            connected = False
            stats.labels_dropped_disconnected += 1
            continue
        if seq is None:
            stats.labels_refused += 1
        else:
            stats.labels_emitted += 1


def serve(  # noqa: PLR0913 — composition root: injected collaborators for the §11 serve-loop test
    cfg: claude_worker.config.ServeConfig,
    *,
    symbol_map: dict[str, int] | None = None,
    iterations: int | None = None,
    http_client: httpx.Client | None = None,
    clock_ns: typing.Callable[[], int] = time.monotonic_ns,
    sleep_fn: typing.Callable[[float], None] = time.sleep,
    rng: random.Random | None = None,
    stats_out: ServeStats | None = None,
) -> int:
    """Run the daemon until SIGTERM/SIGINT (or ``iterations``, tests
    only). Returns the process exit code (0 = clean shutdown).

    ``symbol_map`` (market name -> SymbolId) is the labeling universe;
    the operator wiring for it arrives with the verb layer (item 12) —
    an empty map runs a triage-only daemon (labels impossible, commander
    heartbeats still flow), which is a valid degraded mode.
    """
    stats = ServeStats() if stats_out is None else stats_out
    flag = _StopFlag()
    prev_term = signal.signal(signal.SIGTERM, flag.trip)
    prev_int = signal.signal(signal.SIGINT, flag.trip)

    state = claude_worker.state.State(cfg.db_path)
    uds_client = claude_worker.uds.UdsClient(cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state)
    own_http = http_client is None
    http = httpx.Client() if http_client is None else http_client

    # THE construction site of the SDK client (§5.2).
    llm_client = claude_worker.llm.make_client(cfg)
    complete_fn = claude_worker.llm.complete_fn_for(llm_client)

    watcher = claude_worker.feeds.NewsWatcher(
        state=state,
        feeds=cfg.rss_feeds,
        symbol_map={} if symbol_map is None else symbol_map,
        complete_fn=complete_fn,
        http_client=http,
        rng=rng,
        clock_ns=clock_ns,
    )
    commander = claude_worker.commander.Commander(uds_client, claude_worker.commander.Policy())
    _log.info("serve_started", feeds=len(cfg.rss_feeds), db=str(cfg.db_path))

    try:
        while not flag.stop and (iterations is None or stats.iterations < iterations):
            stats.iterations += 1
            now_ns = clock_ns()
            connected = _ensure_connected(uds_client, commander, stats)
            if connected:
                try:
                    if commander.maybe_heartbeat(now_ns):
                        stats.heartbeats += 1
                except claude_worker.uds.UdsError:
                    stats.uds_errors += 1
                    uds_client.close()
                    connected = False
            poll = watcher.poll_once(now_ns)
            _emit_labels(commander, uds_client, poll.labels, connected, stats)
            if flag.stop or (iterations is not None and stats.iterations >= iterations):
                break
            sleep_fn(TICK_S)
    finally:
        # Shutdown order (§5.2): flush SQLite, close UDS, restore handlers.
        state.close()
        uds_client.close()
        if own_http:
            http.close()
        signal.signal(signal.SIGTERM, prev_term)
        signal.signal(signal.SIGINT, prev_int)
        _log.info(
            "serve_stopped",
            iterations=stats.iterations,
            heartbeats=stats.heartbeats,
            labels_emitted=stats.labels_emitted,
            uds_errors=stats.uds_errors,
        )
    return 0
