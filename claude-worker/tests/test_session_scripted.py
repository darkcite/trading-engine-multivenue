# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Scripted SEMI-MANUAL session test (design §11, 8f amendment).

Executes the ``docs/prompts/ai-session.md`` §4 workflow verb-by-verb as
REAL SUBPROCESSES of the installed ``claude-worker`` entry point, against
the fake UDS server and a canned-report fake harness binary on PATH:

    regime report (0a) -> library list --regime current (0c) ->
    fetch -> positions -> author -> library add -> compose --dry-run ->
    backtest (pass) -> install artifact -> stage -> commit -> push disable

plus the refusal path (failing report -> stage exit 3; commit of an
unstaged hash -> exit 3). ``ANTHROPIC_API_KEY`` is absent from every
subprocess environment — SEMI-MANUAL end-to-end with zero SDK
construction, proven at the process boundary. RG4 extended the pin
deliberately (steps 0a–0c + the library/composer lanes — plan §5.4):
the regime lane degrades honestly without an artifact, the library and
composer run as ``python -m`` module lanes against the same worker db,
and the composed artifact of a single member hashes to the member id.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

import pytest

import claude_worker.backtest
import claude_worker.frames
import tests.conftest

_FIXTURES = pathlib.Path(__file__).parent / "fixtures" / "pmlr"
_RUN_NAME = "run-1755216000000000000"
_SUBPROCESS_TIMEOUT_S = 30.0

# The fake `multivenue-engine` harness (§11 canned reports): computes the
# ruleset hash exactly like the future 8h binary must, emits the pinned
# stdout contract; P&L is steered via FAKE_HARNESS_PNL.
_FAKE_HARNESS = """#!{python}
import hashlib, json, os, pathlib, sys

argv = sys.argv
ruleset = pathlib.Path(argv[argv.index("--ruleset") + 1])
digest = hashlib.sha256(ruleset.read_bytes()).hexdigest()
pnl = float(os.environ.get("FAKE_HARNESS_PNL", "12.5"))
print(json.dumps({{
    "schema_version": 1,
    "ruleset_hash": digest,
    "split": "70/30",
    "oos": {{"net_pnl_usd": pnl, "trades": 80, "trading_days": 3,
             "max_drawdown_usd": 50.0}},
    "bounds": {{"max_order_notional_usd": 50.0,
                "max_symbol_notional_usd": 100.0,
                "max_total_notional_usd": 500.0}},
}}))
"""


def _cli_script() -> pathlib.Path:
    script = pathlib.Path(sys.executable).parent / "claude-worker"
    assert script.is_file(), (
        f"{script} missing — the [project.scripts] entry point should be"
        " installed by `uv run`/`uv sync` (item 12)"
    )
    return script


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
def session_env(
    tmp_path: pathlib.Path, fake_uds: tests.conftest.FakeUdsServer
) -> collections.abc.Iterator[dict[str, str]]:
    """Subprocess environment for the scripted session: full worker env,
    fake harness first on PATH, ANTHROPIC_API_KEY ABSENT."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    harness = bin_dir / "multivenue-engine"
    harness.write_text(_FAKE_HARNESS.format(python=sys.executable))
    harness.chmod(0o755)

    replay = tmp_path / "logs"
    run_dir = replay / _RUN_NAME
    run_dir.mkdir(parents=True)
    shutil.copy(_FIXTURES / "ticks_v2.pmlr", run_dir / "polymarket-ticks.pmlr")
    shutil.copy(_FIXTURES / "fills_v2.pmlr", run_dir / "engine-fills.pmlr")
    (tmp_path / "rulesets").mkdir()

    env = dict(os.environ)
    env.pop("ANTHROPIC_API_KEY", None)  # zero SDK construction, proven
    env["PATH"] = f"{bin_dir}:{env.get('PATH', '')}"
    env["AI_INGRESS_SOCK"] = str(fake_uds.sock_path)
    env["AI_INGRESS_HMAC_KEY"] = tests.conftest.TEST_KEY.hex()
    env["AI_RULESET_DIR"] = str(tmp_path / "rulesets")
    env["CLAUDE_WORKER_REPLAY_DIR"] = str(replay)
    env["CLAUDE_WORKER_DB"] = str(tmp_path / "state.db")
    env["CLAUDE_WORKER_FEATURES_DIR"] = str(tmp_path / "features")
    env["CLAUDE_WORKER_MARKET_MAP"] = str(tmp_path / "market-map.json")
    env["RSS_FEEDS"] = ""
    # RG4 lanes: a private library, regime state dir and an unreachable
    # metrics port — no operator file, no live engine is ever consulted.
    env["CLAUDE_WORKER_LIBRARY_DIR"] = str(tmp_path / "library")
    env["CLAUDE_WORKER_REGIME_DIR"] = str(tmp_path / "regime")
    env["CLAUDE_WORKER_METRICS_URL"] = "http://127.0.0.1:1/metrics"
    env["CLAUDE_WORKER_REGIME_TOML"] = str(tmp_path / "absent-regime.toml")
    env["CLAUDE_WORKER_CANDLES_DB"] = str(tmp_path / "absent-candles.db")
    yield env


def _run(
    env: dict[str, str], *args: str, extra_env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    merged = dict(env)
    if extra_env:
        merged.update(extra_env)
    assert "ANTHROPIC_API_KEY" not in merged
    return subprocess.run(  # noqa: PLW1510 — returncode asserted by callers
        [str(_cli_script()), *args],
        env=merged,
        capture_output=True,
        text=True,
        timeout=_SUBPROCESS_TIMEOUT_S,
    )


def _run_lane(env: dict[str, str], module: str, *args: str) -> subprocess.CompletedProcess[str]:
    """A `python -m claude_worker.<module>` lane (regime / library /
    compose are module lanes, not verbs — the 8-verb surface is frozen)."""
    assert "ANTHROPIC_API_KEY" not in env
    return subprocess.run(  # noqa: PLW1510 — returncode asserted by callers
        [sys.executable, "-m", f"claude_worker.{module}", *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=_SUBPROCESS_TIMEOUT_S,
    )


# One REAL v2 row (the structural parser refuses the bare `{"name"}`
# stub the pre-RG4 script authored — the library stores only what the
# engine would accept), labelled for calm markets.
_MEMBER_ROWS = {
    "rows": [
        {
            "name": "xv-okx-bnspot-vlow",
            "family": "crypto",
            "instrument": "okx:BTC-USDT",
            "ref": "binance:btcusdt",
            "feature": "mid",
            "combine": "diff_bps",
            "abs": True,
            "enter": 3.0,
            "exit": 1.0,
            "regimes": ["vol:low"],
            "horizon_ms": 60000,
            "max_risk_usd": 3000.0,
        }
    ]
}


def test_scripted_session_happy_path(
    session_env: dict[str, str],
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
) -> None:
    """ai-session.md §4, verbatim: regime report -> library list ->
    fetch -> positions -> author -> library add -> compose --dry-run ->
    backtest pass -> install -> stage -> commit -> push disable. Asserts
    the exact wire sequence and the cross-process seq monotonicity of
    the durable allocator."""
    # 0a. the regime report degrades honestly without an artifact (exit 2, a tell, no frame)
    proc = _run_lane(
        session_env, "regime", "report",
        "--regime", session_env["CLAUDE_WORKER_REGIME_TOML"], "--db", session_env["CLAUDE_WORKER_CANDLES_DB"],
    )
    assert proc.returncode == 2, proc.stderr + proc.stdout
    assert "absent" in proc.stderr

    # 0c. what exists for the current words — nothing yet (no engine, no declaration ⇒ UNKNOWN)
    proc = _run_lane(session_env, "library", "list", "--regime", "current")
    assert proc.returncode == 0, proc.stderr + proc.stdout
    assert "library list: regime (unknown)" in proc.stdout and "empty" in proc.stdout

    # 1. fetch (read-only: must send nothing)
    proc = _run(session_env, "fetch")
    assert proc.returncode == 0, proc.stderr
    assert "7.json" in proc.stdout

    # 2. positions (read-only view consulted before acting)
    proc = _run(session_env, "positions", "--json")
    assert proc.returncode == 0, proc.stderr
    assert json.loads(proc.stdout)["positions"], "golden fills expected"

    # 3. author the member rows, add them to the library, compose for the words
    authored = tmp_path / "R.json"
    authored.write_text(json.dumps(_MEMBER_ROWS))
    proc = _run_lane(session_env, "library", "add", "--from", str(authored), "--name", "xv-vlow", "--thesis", "calm-market xv")
    assert proc.returncode == 0, proc.stderr + proc.stdout
    assert "library add: added candidate" in proc.stdout and "labels=[vol:low]" in proc.stdout
    member_id = proc.stdout.split()[4]
    proc = _run_lane(session_env, "library", "list", "--regime", "vol:low", "--status", "candidate")
    assert proc.returncode == 0, proc.stderr + proc.stdout
    assert "fit candidate" in proc.stdout and "xv-vlow" in proc.stdout
    # RG8: an UNLABELLED member may be added (a candidate) but is never
    # validated and never composed — the gate would be a no-op for it.
    bare_rows = {"rows": [{k: v for k, v in _MEMBER_ROWS["rows"][0].items() if k != "regimes"}]}
    bare = tmp_path / "bare.json"
    bare.write_text(json.dumps({"rows": [{**bare_rows["rows"][0], "name": "xv-bare"}]}))
    proc = _run_lane(session_env, "library", "add", "--from", str(bare), "--name", "xv-bare")
    assert proc.returncode == 0, proc.stderr + proc.stdout
    assert "added candidate" in proc.stdout and "labels=ANY" in proc.stdout
    proc = _run_lane(session_env, "library", "validate", "xv-bare")
    assert proc.returncode != 0 and "RG8" in (proc.stderr + proc.stdout)
    proc = _run_lane(session_env, "compose", "--dry-run", "--regime", "vol:low", "--include-candidates", "--json")
    assert proc.returncode == 0, proc.stderr + proc.stdout
    composed = json.loads(proc.stdout)
    assert composed["members"] == [{"member_id": member_id, "name": "xv-vlow", "fit": "word", "rows": 1}]
    assert composed["hash"] == member_id, "a single-member table hashes to its member id"
    ruleset = pathlib.Path(composed["path"])
    assert ruleset.is_file() and ruleset.parent == tmp_path / "compositions"

    # 4. backtest — gates pass, report next to the composed artifact
    proc = _run(session_env, "backtest", "--ruleset", str(ruleset))
    assert proc.returncode == 0, proc.stderr + proc.stdout
    report = claude_worker.backtest.report_path_for(ruleset)
    assert report.is_file()

    # 5. install the artifact under AI_RULESET_DIR/<hash128>.json (§4 step 5)
    full_hash, hash128 = claude_worker.backtest.ruleset_hashes(ruleset)
    shutil.copy(ruleset, tmp_path / "rulesets" / f"{hash128.hex()}.json")
    assert full_hash[:32] == hash128.hex()  # the doc's first-32-hex rule

    # 6. stage / 7. commit / 10. rollback verb
    proc = _run(session_env, "stage-ruleset", "--ruleset", str(ruleset), "--report", str(report))
    assert proc.returncode == 0, proc.stderr + proc.stdout
    assert f"staged {full_hash}" in proc.stdout
    proc = _run(session_env, "commit-ruleset", "--ruleset", str(ruleset))
    assert proc.returncode == 0, proc.stderr + proc.stdout
    proc = _run(session_env, "push", "--kind", "disable", "--strategy", "5")
    assert proc.returncode == 0, proc.stderr + proc.stdout

    # Wire: 3 sending verbs x (heartbeat + payload) = 6 frames, in order.
    _wait_for_frames(fake_uds, 6)
    assert fake_uds.errors == []
    kinds = [fake_uds.cmd_field(i, "kind") for i in range(6)]
    assert kinds == [
        claude_worker.frames.KIND_HEARTBEAT,
        claude_worker.frames.KIND_RULESET_STAGE,
        claude_worker.frames.KIND_HEARTBEAT,
        claude_worker.frames.KIND_RULESET_COMMIT,
        claude_worker.frames.KIND_HEARTBEAT,
        claude_worker.frames.KIND_DISABLE_STRATEGY,
    ]
    assert fake_uds.cmd_field(5, "strategy_id") == 5  # the vm slot
    px = int.from_bytes(bytes.fromhex(full_hash)[0:8], "little", signed=True)
    assert fake_uds.cmd_field(1, "px") == px
    assert fake_uds.cmd_field(3, "px") == px
    # Durable seq allocator: strictly increasing ACROSS processes.
    seqs = [fake_uds.cmd_field(i, "seq") for i in range(6)]
    assert seqs == sorted(seqs) and len(set(seqs)) == 6


def test_scripted_session_refusal_path(
    session_env: dict[str, str],
    fake_uds: tests.conftest.FakeUdsServer,
    tmp_path: pathlib.Path,
) -> None:
    """The §11 refusal legs: failing report -> backtest exit 3 and stage
    exit 3; commit of an unstaged hash -> exit 3."""
    ruleset = tmp_path / "R.json"
    ruleset.write_text('{"rows": [{"name": "loser"}]}')

    proc = _run(
        session_env, "backtest", "--ruleset", str(ruleset), extra_env={"FAKE_HARNESS_PNL": "-5.0"}
    )
    assert proc.returncode == 3, proc.stderr + proc.stdout
    report = claude_worker.backtest.report_path_for(ruleset)
    assert report.is_file()  # report written on gate fail (§6)

    proc = _run(session_env, "stage-ruleset", "--ruleset", str(ruleset), "--report", str(report))
    assert proc.returncode == 3, proc.stderr + proc.stdout

    proc = _run(session_env, "commit-ruleset", "--hash", "ee" * 32)
    assert proc.returncode == 3, proc.stderr + proc.stdout

    # Refusals put nothing but heartbeats on the wire.
    time.sleep(0.05)
    kinds = {fake_uds.cmd_field(i, "kind") for i in range(len(fake_uds.frames))}
    assert kinds <= {claude_worker.frames.KIND_HEARTBEAT}
