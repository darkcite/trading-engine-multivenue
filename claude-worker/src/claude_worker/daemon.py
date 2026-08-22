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

8h §9: one new collaborator joins beside the watcher — [`ResearchCycle`]
(owns the §7.4 cycle state machine + the §8.1 promote step), due every
``CLAUDE_WORKER_STRATEGIST_INTERVAL_S`` and checked once per tick. The
§7.6 threading law, enforced here: the slow work (the fetch subprocess
and the Fable-5 call) runs on a single background worker thread
(``ThreadPoolExecutor(max_workers=1)``) that writes FILES only — its one
SQLite touch is ``strategist.call_with_cache``'s own handle on the
``prompt_cache`` table; every UDS send (heartbeat, Stage, Commit) and
every events-ledger row stays on THIS loop thread; the backtest
subprocess is fast relative to cadence and runs inline. SIGTERM drain:
an in-flight background call is abandoned (``shutdown(wait=False,
cancel_futures=True)``; its file-write is atomic-or-absent), an
in-flight promote finishes its current frame before close (sends are
synchronous on this thread).

Convention: full ``import x`` only. No ``from x import y``.
"""

import concurrent.futures
import dataclasses
import json
import os
import pathlib
import random
import signal
import subprocess
import sys
import time
import types
import typing

import httpx
import structlog

import claude_worker.backtest
import claude_worker.commander
import claude_worker.config
import claude_worker.features
import claude_worker.feeds
import claude_worker.fetchers
import claude_worker.labeling
import claude_worker.llm
import claude_worker.state
import claude_worker.strategist
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


# ---- 8h research cycle (design §7.4/§7.6/§8.1/§9) ------------------------

# Default wall-clock timeout for the in-cycle fetch subprocess: REST is
# best-effort enrichment (§6.1 doctrine) — a hung fetch must not stall
# the cycle past one interval.
FETCH_TIMEOUT_S: float = 180.0

_SPLIT_DEFAULT: str = "70/30"

# maybe_run phases (one research cycle spans many 0.2 s ticks).
_IDLE: str = "idle"
_FETCH: str = "fetch"
_CALL: str = "call"
_PROMOTE: str = "promote"

_PURPOSE_PROPOSAL: str = "proposal"
_PURPOSE_REVISION: str = "revision"


@dataclasses.dataclass(slots=True)
class ResearchStats:
    """Research-cycle counters (test/diagnostic surface, the ServeStats
    pattern)."""

    cycles_started: int = 0
    skips_no_capture: int = 0
    skips_budget: int = 0
    fetch_failures: int = 0
    calls: int = 0
    dedupe_hits: int = 0
    call_failures: int = 0
    candidates_rejected: int = 0
    backtests: int = 0
    backtest_errors: int = 0
    gate_failures: int = 0
    promotions: int = 0
    promote_retries: int = 0


def default_fetch_fn() -> bool:
    """One in-cycle ``claude-worker fetch --news`` SUBPROCESS (§7.4 step
    1). The verb never touches the socket, so it cannot collide with the
    serve-held UDS slot; a failure is a counted degradation — the cycle
    proceeds on the files that exist. Runs on the background worker
    (file-writing work, §7.6-legal). The entry-point script lives beside
    the interpreter (the ``test_session_scripted`` invocation pattern)."""
    script = pathlib.Path(sys.executable).parent / "claude-worker"
    if not script.is_file():
        _log.warning("research_fetch_script_missing", script=str(script))
        return False
    try:
        proc = subprocess.run(  # noqa: PLW1510 — returncode checked below
            [str(script), "fetch", "--news"],
            capture_output=True,
            text=True,
            timeout=FETCH_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        _log.warning("research_fetch_failed", error=str(exc))
        return False
    if proc.returncode != 0:
        _log.warning("research_fetch_exit", code=proc.returncode, stderr=proc.stderr[-300:])
        return False
    return True


def _gate_summary(gates: claude_worker.backtest.GateResult) -> str:
    return (
        f"pnl_positive={gates.pnl_positive} min_trades={gates.min_trades}"
        f" min_days={gates.min_days} max_drawdown={gates.max_drawdown}"
        f" bounds={gates.bounds} -> {'PASS' if gates.all_passed else 'FAIL'}"
    )


class ResearchCycle:
    """The §7.4 cycle state machine, one instance per serve run.

    Cadence: primed at construction — the FIRST cycle is due one full
    interval after serve start (a restart can never burn budget; the
    SQLite dedupe additionally makes any repeated inputs free). Cycles
    never overlap: due-ness is only consulted in the idle phase, which
    also caps the background executor at one in-flight job.

    Single-writer law (§7.6): ``maybe_run`` executes on the serve-loop
    thread; the injected executor runs ONLY the fetch subprocess and
    ``strategist.call_with_cache``. Frames and events never leave this
    thread.
    """

    def __init__(  # noqa: PLR0913 — composition root: injected collaborators, ServeStats pattern
        self,
        state: claude_worker.state.State,
        cfg: claude_worker.config.BaseConfig,
        markets: dict[str, int],
        complete_fn: claude_worker.strategist.CompleteFn,
        executor: concurrent.futures.Executor,
        *,
        fetch_fn: typing.Callable[[], bool] | None = None,
        run_backtest_fn: typing.Callable[..., claude_worker.backtest.BacktestOutcome] | None = None,
        env: typing.Mapping[str, str] | None = None,
        clock_ns: typing.Callable[[], int] = time.monotonic_ns,
        wall_ns: typing.Callable[[], int] = time.time_ns,
        stats: ResearchStats | None = None,
    ) -> None:
        self._state = state
        self._cfg = cfg
        self._markets = markets
        self._complete_fn = complete_fn
        self._executor = executor
        self._fetch_fn = default_fetch_fn if fetch_fn is None else fetch_fn
        self._run_backtest_fn = (
            claude_worker.backtest.run_backtest if run_backtest_fn is None else run_backtest_fn
        )
        # §7.5 env keys, read at the seam ONCE at composition (strict
        # parse fails the boot, fail-fast — the ServeConfig pattern).
        self._interval_ns: int = claude_worker.strategist.interval_s(env) * 1_000_000_000
        self._daily_cap: int = claude_worker.strategist.daily_cap(env)
        self._wall_ns = wall_ns
        self.stats: ResearchStats = ResearchStats() if stats is None else stats
        # Cadence primes at construction: first due = start + interval.
        self._next_due_ns: int = clock_ns() + self._interval_ns
        self._phase: str = _IDLE
        self._future: concurrent.futures.Future[typing.Any] | None = None
        self._last_run_name: str | None = None
        # In-cycle context.
        self._run_name: str | None = None
        self._digest: str = ""
        self._purpose: str = _PURPOSE_PROPOSAL
        self._pending: tuple[
            claude_worker.strategist.Candidate, claude_worker.backtest.BacktestOutcome
        ] | None = None

    # -- helpers (serve-loop thread only) --

    def _event(self, kind: str, detail: dict[str, object]) -> None:
        self._state.record_event(
            kind, json.dumps(detail, sort_keys=True, separators=(",", ":"))
        )

    def _finish_cycle(self) -> None:
        self._phase = _IDLE
        self._future = None
        self._pending = None
        self._digest = ""
        self._run_name = None
        self._purpose = _PURPOSE_PROPOSAL

    def _submit_call(self, prompt: str, purpose: str) -> bool:
        """Budget-gate (§7.5, serve-side — the events ledger is loop-only
        under §7.6) then submit the background call. False = budget skip
        (cycle over)."""
        now_ns = self._wall_ns()
        burned = claude_worker.strategist.calls_today(self._state, now_ns)
        if burned >= self._daily_cap:
            self._event(
                claude_worker.strategist.EVENT_BUDGET_SKIP,
                {"calls_today": burned, "daily_cap": self._daily_cap, "purpose": purpose},
            )
            self.stats.skips_budget += 1
            _log.info("strategist_budget_skip", calls_today=burned, cap=self._daily_cap)
            return False
        self._purpose = purpose
        self._future = self._executor.submit(
            claude_worker.strategist.call_with_cache,
            self._cfg.db_path,
            prompt,
            self._complete_fn,
        )
        self._phase = _CALL
        return True

    def _start_cycle(self) -> None:
        latest = claude_worker.features.latest_run_dir(self._cfg.replay_dir)
        if latest is None or latest.name == self._last_run_name:
            self._event(
                claude_worker.strategist.EVENT_CAPTURE_SKIP,
                {"latest": None if latest is None else latest.name},
            )
            self.stats.skips_no_capture += 1
            _log.info(
                "strategist_capture_skip",
                latest=None if latest is None else latest.name,
            )
            return
        self._last_run_name = latest.name
        self._run_name = latest.name
        self.stats.cycles_started += 1
        _log.info("research_cycle_start", run=latest.name)
        self._future = self._executor.submit(self._fetch_fn)
        self._phase = _FETCH

    def _after_fetch(self, fetch_ok: bool) -> None:
        if not fetch_ok:
            self.stats.fetch_failures += 1
        universe: list[int] | None
        try:
            latest = claude_worker.features.latest_run_dir(self._cfg.replay_dir)
            universe = (
                None
                if latest is None
                else sorted(claude_worker.fetchers.observed_universe(latest))
            )
        except (OSError, ValueError):
            universe = None
        self._digest = claude_worker.strategist.build_digest(
            self._cfg.features_dir,
            self._run_name,
            self._markets,
            universe=universe,
        )
        prompt = claude_worker.strategist.build_user_prompt(self._digest)
        if not self._submit_call(prompt, _PURPOSE_PROPOSAL):
            self._finish_cycle()

    def _record_call(self, result: claude_worker.strategist.CallResult) -> None:
        """§7.5 ledger row — ONLY for a real API call; a SQLite dedupe
        hit is zero API cost and burns no budget."""
        if result.sqlite_cache_hit or result.completion is None:
            self.stats.dedupe_hits += 1
            _log.info("strategist_dedupe_hit", purpose=self._purpose)
            return
        self.stats.calls += 1
        self._state.record_event(
            claude_worker.strategist.EVENT_STRATEGIST_CALL,
            claude_worker.strategist.call_detail(result.completion, self._purpose),
        )

    def _reject(self, raw: str, reason: str) -> None:
        path = claude_worker.strategist.archive_rejected(
            claude_worker.strategist.candidates_dir(self._cfg.db_path), raw
        )
        self._event(
            claude_worker.strategist.EVENT_CANDIDATE_REJECTED,
            {"reason": reason, "archived": str(path), "purpose": self._purpose},
        )
        self.stats.candidates_rejected += 1
        _log.info("strategist_candidate_rejected", reason=reason, archived=str(path))

    def _after_call(self, result: claude_worker.strategist.CallResult) -> None:
        self._record_call(result)
        proposal = claude_worker.strategist.parse_proposal(result.text)
        if proposal is None:
            self._reject(result.text, "malformed_output")
            self._finish_cycle()
            return
        candidate = claude_worker.strategist.write_candidate(
            claude_worker.strategist.candidates_dir(self._cfg.db_path), proposal
        )
        self.stats.backtests += 1
        try:
            outcome = self._run_backtest_fn(
                candidate.path, self._cfg.replay_dir, split=_SPLIT_DEFAULT
            )
        except claude_worker.backtest.BacktestError as exc:
            # Untrusted report / validator reject: candidate-fatal, no
            # revision (a revision needs a REAL gate report to carry).
            self._event(
                claude_worker.strategist.EVENT_CANDIDATE_REJECTED,
                {"reason": "backtest_error", "error": str(exc), "candidate": str(candidate.path)},
            )
            self.stats.backtest_errors += 1
            _log.warning("strategist_backtest_error", error=str(exc))
            self._finish_cycle()
            return
        if outcome.all_passed:
            # §8.1 order: gates PASS => install now; frames when connected.
            claude_worker.strategist.install_candidate(
                self._cfg.ai_ruleset_dir, candidate.path, candidate.hash128_hex
            )
            self._pending = (candidate, outcome)
            self._phase = _PROMOTE
            return
        self.stats.gate_failures += 1
        if self._purpose == _PURPOSE_PROPOSAL:
            # §7.4 call #2: revision carries the gate summary + report.
            prompt = claude_worker.strategist.build_revision_prompt(
                self._digest,
                claude_worker.strategist.artifact_bytes(proposal.rows).decode(),
                _gate_summary(outcome.gates),
                outcome.report_path.read_text(),
            )
            if not self._submit_call(prompt, _PURPOSE_REVISION):
                self._finish_cycle()
            return
        # Revision failed too: archive with report (both already sit in
        # the candidates dir — candidate + R.report.json), cycle over.
        self._event(
            claude_worker.strategist.EVENT_CANDIDATE_REJECTED,
            {
                "reason": "gates_failed_final",
                "candidate": str(candidate.path),
                "report": str(outcome.report_path),
                "gates": _gate_summary(outcome.gates),
            },
        )
        _log.info("strategist_gates_failed_final", candidate=str(candidate.path))
        self._finish_cycle()

    def _try_promote(self, uds_client: claude_worker.uds.UdsClient) -> None:
        """§8.1 steps 2-3 on the serve-loop thread: the FROZEN
        stage/commit pair (+ the §8.2 attribution upsert between them —
        ``state.stage_ruleset``'s additive params; ``backtest.py`` is
        untouched). Disconnected => wait; UdsError => retry next tick
        (Stage supersede semantics make the retry idempotent)."""
        assert self._pending is not None  # phase invariant
        candidate, outcome = self._pending
        if not uds_client.connected:
            return
        try:
            _seq, full_hash = claude_worker.backtest.stage_ruleset(
                self._state, uds_client, candidate.path, outcome.report_path, "auto"
            )
            self._state.stage_ruleset(
                full_hash,
                str(candidate.path),
                str(outcome.report_path),
                "auto",
                model=claude_worker.config.MODEL_STRATEGIST,
                thesis=candidate.thesis,
            )
            claude_worker.backtest.commit_ruleset(self._state, uds_client, full_hash)
        except claude_worker.uds.UdsError as exc:
            self.stats.promote_retries += 1
            uds_client.close()
            _log.warning("strategist_promote_retry", error=str(exc))
            return
        self._event(
            claude_worker.strategist.EVENT_PROMOTION,
            {
                "hash": full_hash,
                "hash128": candidate.hash128_hex,
                "model": claude_worker.config.MODEL_STRATEGIST,
                "candidate": str(candidate.path),
                "oos_net_pnl_usd": outcome.harness.oos_net_pnl_usd,
                "oos_trades": outcome.harness.oos_trades,
            },
        )
        self.stats.promotions += 1
        _log.info("strategist_promoted", hash128=candidate.hash128_hex)
        self._finish_cycle()

    def maybe_run(self, now_ns: int, uds_client: claude_worker.uds.UdsClient) -> None:
        """One per-tick advance (§9: checked once per tick, watcher
        pattern). Non-blocking: background futures are polled, never
        awaited."""
        if self._phase == _IDLE:
            if now_ns < self._next_due_ns:
                return
            self._next_due_ns = now_ns + self._interval_ns
            self._start_cycle()
            return
        if self._phase == _PROMOTE:
            self._try_promote(uds_client)
            return
        future = self._future
        if future is None or not future.done():
            return
        self._future = None
        if self._phase == _FETCH:
            try:
                fetch_ok = bool(future.result())
            except Exception as exc:  # noqa: BLE001 — bg fetch is best-effort by doctrine
                _log.warning("research_fetch_raised", error=str(exc))
                fetch_ok = False
            self._after_fetch(fetch_ok)
            return
        # _CALL
        try:
            result = typing.cast(claude_worker.strategist.CallResult, future.result())
        except Exception as exc:  # noqa: BLE001 — API/transport failure is an expected mode (§5.1 no-crash)
            self._event(
                claude_worker.strategist.EVENT_CALL_FAILED,
                {"error": str(exc), "purpose": self._purpose},
            )
            self.stats.call_failures += 1
            _log.warning("strategist_call_failed", error=str(exc))
            self._finish_cycle()
            return
        self._after_call(result)


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
    research_env: typing.Mapping[str, str] | None = None,
    research_fetch_fn: typing.Callable[[], bool] | None = None,
    research_run_backtest_fn: typing.Callable[..., claude_worker.backtest.BacktestOutcome]
    | None = None,
    research_stats_out: ResearchStats | None = None,
) -> int:
    """Run the daemon until SIGTERM/SIGINT (or ``iterations``, tests
    only). Returns the process exit code (0 = clean shutdown).

    ``symbol_map`` (market name -> SymbolId) is the labeling universe;
    the operator wiring for it arrives with the verb layer (item 12) —
    an empty map runs a triage-only daemon (labels impossible, commander
    heartbeats still flow), which is a valid degraded mode.

    The ``research_*`` keywords (8h, additive) are test seams for the
    [`ResearchCycle`] collaborator; production leaves them None
    (env = ``os.environ``, fetch = the verb subprocess, backtest = the
    frozen ``backtest.run_backtest`` over the real binary).
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

    # 8h §7.6: the ONE background worker (LLM call + fetch subprocess).
    # Threads spawn lazily — a serve run whose cycle never comes due
    # (every pre-8h test) starts zero threads.
    executor = concurrent.futures.ThreadPoolExecutor(
        max_workers=1, thread_name_prefix="cw-research"
    )

    def strategist_complete(
        system: list[dict[str, object]], prompt: str
    ) -> claude_worker.llm.Completion:
        # §7.2: MODEL_STRATEGIST's first consumer; its own token budget.
        return claude_worker.llm.complete_message(
            llm_client,
            claude_worker.config.MODEL_STRATEGIST,
            prompt,
            max_tokens=claude_worker.llm.STRATEGIST_MAX_TOKENS,
            system=system,
        )

    research = ResearchCycle(
        state,
        cfg,
        {} if symbol_map is None else symbol_map,
        strategist_complete,
        executor,
        fetch_fn=research_fetch_fn,
        run_backtest_fn=research_run_backtest_fn,
        env=os.environ if research_env is None else research_env,
        clock_ns=clock_ns,
        stats=research_stats_out,
    )
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
            # 8h §9: research cycle, checked once per tick. Frames stay
            # on THIS thread (§7.6); `uds_client.connected` is the live
            # truth after any emit-path close above.
            research.maybe_run(now_ns, uds_client)
            if flag.stop or (iterations is not None and stats.iterations >= iterations):
                break
            sleep_fn(TICK_S)
    finally:
        # Shutdown order (§5.2 + §9 drain): abandon any in-flight
        # background call (its file-write is atomic-or-absent), flush
        # SQLite, close UDS, restore handlers.
        executor.shutdown(wait=False, cancel_futures=True)
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
