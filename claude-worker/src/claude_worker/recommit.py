# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Post-boot ruleset re-commit — operator ruling #7(b) (mvp-plan §8
item 7, 2026-08-23; landed by the 2026-08-28 capture-remediation plan).

A committed ruleset's table lives IN-MEMORY inside the engine — every
restart clears it, and with the T2 slot restarts the standing lane now
restarts 3× per UTC day. This MODULE (never a verb — the 8-verb
surface is frozen; ``iv_digest`` precedent) restores the AI lane after
each boot:

1. wait for the engine's ``ai.sock`` to exist (bounded);
2. look up the most recently COMMITTED, gates-passed registry row
   (``state.committed_rulesets()[0]`` — the §8.3 active/prior source);
3. re-stage it from its BOUND paths (ruleset + report; the §6 gate
   binding recomputes the hash and re-verifies gates — NO override
   exists on this path either);
4. guard: the recomputed hash must equal the registry row's hash (a
   drifted file under a bound path is a refusal, never a silent
   promotion of different bytes);
5. re-commit that hash (mask-gated at the vm member — the 8g gating
   pin; the standing boot mask enables vm, so a normal boot accepts).

No committed row is an honest no-op (exit 0) — fresh installs and
post-rollback-disable states boot clean. Exit codes mirror the §6
verb convention: 0 OK/no-op · 2 usage · 3 gate refused · 4 transport
(sock absent/timeout/HMAC) · 5 state · 1 unexpected (fail-fast).

Invocation: ``python -m claude_worker.recommit`` from
``scripts/recommit-ruleset.sh`` (engine-wrapper's per-boot child),
serialized behind the global worker law by that script. Offline
orchestration — the hot-path doctrine does not apply here; the module
holds no Anthropic client and never reads ``ANTHROPIC_API_KEY``.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import pathlib
import sys
import time
import typing

import claude_worker.backtest
import claude_worker.config
import claude_worker.state
import claude_worker.uds

EXIT_OK: int = 0
EXIT_USAGE: int = 2
EXIT_GATE: int = 3
EXIT_TRANSPORT: int = 4
EXIT_STATE: int = 5

_SOCK_POLL_S: float = 1.0
#: Transport-retry cadence (VM2 V8 outage fix, see ``main``).
_RETRY_POLL_S: float = 2.0
_AUTHOR_MODE: str = "session"  # semi-manual lane (operator ruling #5)


def wait_for_sock(sock_path: pathlib.Path, budget_s: float) -> bool:
    """Poll for the UDS path to exist, up to ``budget_s`` seconds.

    Existence is NOT readiness: the engine's PREVIOUS boot leaves a
    stale socket inode behind, so this returns True immediately on
    every restart while ``connect()`` still refuses until the new
    engine binds. The 2026-08-30→09-01 outage (four boots, VM inert,
    parity window lost) was exactly that race — which is why ``main``
    now RETRIES transport failures against the same budget instead of
    aborting on the first refused connect."""
    deadline = time.monotonic() + budget_s
    while time.monotonic() < deadline:
        if sock_path.exists():
            return True
        time.sleep(_SOCK_POLL_S)
    return sock_path.exists()


def recommit_active(
    cfg: claude_worker.config.BaseConfig,
    report: typing.Callable[[str], None] = print,
) -> int:
    """Steps 2-5 above against an already-present socket. Returns an
    exit code; raises nothing that ``main`` doesn't translate."""
    state = claude_worker.state.State(cfg.db_path)
    try:
        rows = state.committed_rulesets()
        if not rows:
            report("recommit: no committed ruleset in the registry — honest no-op")
            return EXIT_OK
        full_hash, path, report_path, _staged_ts, _committed_ts = rows[0]
        if report_path is None:
            report(
                f"recommit: registry row {full_hash[:12]} has no bound report"
                " — cannot re-stage (gate binding needs the report); refusing"
            )
            return EXIT_GATE
        ruleset_file = pathlib.Path(path)
        report_file = pathlib.Path(report_path)
        if not ruleset_file.is_file() or not report_file.is_file():
            report(
                f"recommit: bound paths missing for {full_hash[:12]}"
                f" (ruleset={ruleset_file} report={report_file}) — refusing"
            )
            return EXIT_GATE
        client = claude_worker.uds.UdsClient(
            cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state
        )
        client.connect()
        try:
            client.send_heartbeat()
            seq_stage, staged_hash = claude_worker.backtest.stage_ruleset(
                state, client, ruleset_file, report_file, _AUTHOR_MODE
            )
            if staged_hash != full_hash:
                # The file under the bound path is no longer the bytes
                # the registry committed — refuse loudly; a re-commit
                # of the OLD hash would now point at a phantom stage.
                report(
                    "recommit: bound ruleset file drifted —"
                    f" registry {full_hash[:12]} vs on-disk {staged_hash[:12]};"
                    " refusing (operator decides)"
                )
                return EXIT_GATE
            seq_commit = claude_worker.backtest.commit_ruleset(state, client, full_hash)
        finally:
            client.close()
    finally:
        state.close()
    report(
        f"recommit: re-staged (seq={seq_stage}) + re-committed (seq={seq_commit})"
        f" {full_hash}"
    )
    return EXIT_OK


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="claude_worker.recommit")
    parser.add_argument(
        "--wait-sock-seconds",
        type=float,
        default=180.0,
        help="How long to wait for ai.sock to appear (engine boot).",
    )
    args = parser.parse_args(argv)
    if args.wait_sock_seconds < 0:
        print("recommit: --wait-sock-seconds must be >= 0", file=sys.stderr)
        return EXIT_USAGE
    try:
        cfg = claude_worker.config.load_base_from_env()
    except ValueError as exc:
        print(f"recommit: config: {exc}", file=sys.stderr)
        return EXIT_USAGE
    deadline = time.monotonic() + args.wait_sock_seconds
    if not wait_for_sock(cfg.ai_ingress_sock, args.wait_sock_seconds):
        print(
            f"recommit: ai.sock never appeared at {cfg.ai_ingress_sock}"
            f" within {args.wait_sock_seconds:.0f}s — giving up (next boot retries)",
            file=sys.stderr,
        )
        return EXIT_TRANSPORT
    # VM2 V8 outage fix (2026-09-02; root-caused 2026-08-31): a STALE
    # socket inode from the previous boot satisfies the existence wait
    # instantly, then connect() gets ECONNREFUSED until the new engine
    # binds — four consecutive boots aborted here and the VM ran inert
    # for ~2.5 days. Transport failures now RETRY against the SAME
    # --wait-sock-seconds budget (one final attempt at the deadline);
    # gate/state refusals stay immediate — only transport is a race.
    while True:
        try:
            return recommit_active(cfg)
        except claude_worker.uds.UdsError as exc:
            if time.monotonic() >= deadline:
                print(
                    f"recommit: transport: {exc} — budget exhausted"
                    f" ({args.wait_sock_seconds:.0f}s); giving up (next boot retries)",
                    file=sys.stderr,
                )
                return EXIT_TRANSPORT
            print(
                f"recommit: transport: {exc} — engine not bound yet, retrying",
                file=sys.stderr,
            )
            time.sleep(_RETRY_POLL_S)
        except claude_worker.backtest.GateRefused as exc:
            print(
                f"recommit: gate refused (final — no override exists): {exc}",
                file=sys.stderr,
            )
            return EXIT_GATE
        except claude_worker.state.StateError as exc:
            print(f"recommit: state: {exc}", file=sys.stderr)
            return EXIT_STATE
    # Anything else propagates — fail-fast, traceback to the log.


if __name__ == "__main__":
    sys.exit(main())
