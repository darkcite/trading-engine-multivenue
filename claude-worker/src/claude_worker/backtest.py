# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""backtester (design §5.1): subprocess seam to ``multivenue-engine
backtest`` + the GATES.

The harness binary ships in 8h — the seam is mockable NOW (§5.1): the
runner is an injected ``run_fn(argv) -> stdout`` and every test feeds it
canned reports (plan §11 pattern). The default ``run_fn`` shells out to
the real binary once it exists; nothing else in the worker knows a
subprocess is involved.

Gates are code + numbers in worker config, NEVER prompts (§5.1):
OOS net P&L > 0 after fees+latency; >= 50 trades over >= 2 trading
days; max drawdown <= cap; strategy bounds <= risk-policy caps
(docs/risk-policy.md: $100/order, $250/symbol, $1 000 total; DD cap
mirrors the $200/day realized-loss kill line).

Trust chain (feeds the item-12 ``stage-ruleset`` binding): this module
recomputes the ruleset's full SHA-256 from the file bytes, REQUIRES the
harness report to carry the same hash and our schema version, and writes
the worker report (gates + hash + verdict) next to the ruleset. A gate
fail still writes the report (§6: exit 3, report written) — it is never
stageable because ``gates.all_passed`` is false and stage-ruleset has no
override flag.

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import hashlib
import json
import pathlib
import subprocess
import time
import typing

import claude_worker.frames
import claude_worker.state
import claude_worker.uds

# The engine binary name (PATH-resolved by default; absolute in prod .env
# wiring arrives with the 8h harness).
ENGINE_BINARY: str = "multivenue-engine"

# Version of BOTH the harness stdout contract and the worker report file.
# stage-ruleset (item 12) refuses any other version.
REPORT_SCHEMA_VERSION: int = 1

REPORT_SUFFIX: str = ".report.json"

HASH128_LEN: int = 16

# VM2 V5 — the D-3 gate amendment (operator ruling 2026-08-29, the
# D1-pattern frozen-surface amendment; recorded in docs/vm2-plan.md §8
# V0 entry): position rulesets do FEW round-trips, so `min_trades`
# counts LEGS (fills — additive report key, defaulting to oos.trades
# for pre-V5 reports) and position rulesets ADDITIONALLY require
# `round_trips >= MIN_ROUND_TRIPS`. GateThresholds/GateResult keep
# their frozen shapes — the requirement folds into the `min_trades`
# verdict; a report with `position_rows == 0` gates exactly as before.
MIN_ROUND_TRIPS: int = 10


class BacktestError(Exception):
    """Harness failure or contract violation: bad exit, garbage stdout,
    schema/hash mismatch. Fail-fast — a report we cannot trust must never
    reach the gates."""


class GateRefused(Exception):
    """Stage/commit binding refusal (§6 exit 3). FINAL by design: no
    override flag exists anywhere in the CLI surface — fix the ruleset,
    don't fight the gate."""


@dataclasses.dataclass(frozen=True, slots=True)
class GateThresholds:
    """§5.1 gate numbers. Defaults pin the design text + risk-policy
    caps; operators tighten (never loosen past risk-policy) via worker
    config wiring in item 12."""

    min_oos_net_pnl_usd: float = 0.0  # strict >
    min_trades: int = 50
    # Operator ruling 2026-08-30 (D1-pattern frozen-surface amendment,
    # cited in the pin tests): MVP tempo — the OOS trading-day floor
    # drops 2 → 1 so a ~12 h capture-age wait suffices for staging.
    # Trade-off accepted on record: at floor 1 the OOS verdict can
    # come from a single day's regime (the old floor was the
    # single-regime-overfit guard). Revisit at the M6 soak.
    min_trading_days: int = 1
    # Operator ruling 2026-08-29 (D1-pattern frozen-surface amendment,
    # cited in the pin tests): the $50k-book research tier — DD gate
    # 15% of book, per-order $10k, per-sym $20k, total $100k (2x
    # book). The $1k demo tier is recorded superseded in
    # docs/risk-policy.md.
    max_drawdown_usd: float = 7_500.0
    max_order_notional_usd: float = 10_000.0
    max_symbol_notional_usd: float = 20_000.0
    max_total_notional_usd: float = 100_000.0


class GateResult(typing.NamedTuple):
    """Per-gate verdicts (report ``gates`` section)."""

    pnl_positive: bool
    min_trades: bool
    min_days: bool
    max_drawdown: bool
    bounds: bool

    @property
    def all_passed(self) -> bool:
        return all(self)


class HarnessReport(typing.NamedTuple):
    """Validated machine-readable harness output (stdout contract).

    The VM2 V5 fields (`oos_round_trips`, `oos_legs`,
    `position_rows`) are ADDITIVE schema-1 keys — absent on pre-V5
    reports, where they default to the exact pre-V5 semantics
    (legs = trades, no position gating). D-3 ruling cited above.
    """

    ruleset_hash: str
    split: str
    oos_net_pnl_usd: float
    oos_trades: int
    oos_trading_days: int
    oos_max_drawdown_usd: float
    max_order_notional_usd: float
    max_symbol_notional_usd: float
    max_total_notional_usd: float
    oos_round_trips: int = 0
    oos_legs: int = -1  # -1 = absent ⇒ legs := trades
    position_rows: int = 0


class BacktestOutcome(typing.NamedTuple):
    """What ``run_backtest`` hands back to verbs/serve: verdict + the
    written report."""

    all_passed: bool
    report_path: pathlib.Path
    gates: GateResult
    harness: HarnessReport


def ruleset_hashes(ruleset_path: pathlib.Path) -> tuple[str, bytes]:
    """(full sha256 hex, hash128 first-16-bytes) over the canonical file
    bytes — §13 decision 5: hash128 rides px+qty in Stage/Commit frames;
    the full hash lives in report + registry."""
    digest = hashlib.sha256(ruleset_path.read_bytes()).digest()
    return digest.hex(), digest[:HASH128_LEN]


def _strict_float(obj: dict[str, object], key: str) -> float:
    value = obj.get(key)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise BacktestError(f"harness report: {key} must be a number, got {value!r}")
    return float(value)


def _strict_int(obj: dict[str, object], key: str) -> int:
    value = obj.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        raise BacktestError(f"harness report: {key} must be an integer, got {value!r}")
    return value


def _lenient_int(obj: dict[str, object], key: str, default: int) -> int:
    """VM2 V5 additive keys: absent ⇒ default (pre-V5 reports);
    present ⇒ integer or the report is untrustworthy."""
    if key not in obj:
        return default
    return _strict_int(obj, key)


def _section(obj: dict[str, object], key: str) -> dict[str, object]:
    value = obj.get(key)
    if not isinstance(value, dict):
        raise BacktestError(f"harness report: missing section {key!r}")
    return typing.cast(dict[str, object], value)


def parse_harness_report(stdout: str, expected_hash: str) -> HarnessReport:
    """STRICT parse+validation of harness stdout. The hash equality check
    is the anti-drift bind: the report must describe exactly the file we
    hashed, or nothing downstream may trust it."""
    try:
        raw = json.loads(stdout)
    except ValueError as exc:
        raise BacktestError(f"harness stdout is not JSON: {exc}") from exc
    if not isinstance(raw, dict):
        raise BacktestError("harness stdout is not a JSON object")
    obj = typing.cast(dict[str, object], raw)
    version = obj.get("schema_version")
    if version != REPORT_SCHEMA_VERSION:
        raise BacktestError(f"harness report schema_version {version!r} != {REPORT_SCHEMA_VERSION}")
    ruleset_hash = obj.get("ruleset_hash")
    if ruleset_hash != expected_hash:
        raise BacktestError("harness report ruleset_hash does not match the ruleset file")
    split = obj.get("split")
    if not isinstance(split, str):
        raise BacktestError("harness report: split must be a string")
    oos = _section(obj, "oos")
    bounds = _section(obj, "bounds")
    return HarnessReport(
        ruleset_hash=ruleset_hash,
        split=split,
        oos_net_pnl_usd=_strict_float(oos, "net_pnl_usd"),
        oos_trades=_strict_int(oos, "trades"),
        oos_trading_days=_strict_int(oos, "trading_days"),
        oos_max_drawdown_usd=_strict_float(oos, "max_drawdown_usd"),
        max_order_notional_usd=_strict_float(bounds, "max_order_notional_usd"),
        max_symbol_notional_usd=_strict_float(bounds, "max_symbol_notional_usd"),
        max_total_notional_usd=_strict_float(bounds, "max_total_notional_usd"),
        oos_round_trips=_lenient_int(oos, "round_trips", 0),
        oos_legs=_lenient_int(oos, "legs", -1),
        position_rows=_lenient_int(obj, "position_rows", 0),
    )


def evaluate_gates(report: HarnessReport, thresholds: GateThresholds) -> GateResult:
    """The §5.1 gate matrix — pure code, no prompts, no overrides.

    VM2 V5 (D-3, ruling cited at MIN_ROUND_TRIPS): `min_trades`
    counts LEGS (= trades on pre-V5 reports) and folds the
    position-ruleset round-trip floor in — GateResult keeps its
    frozen five-field shape."""
    legs = report.oos_trades if report.oos_legs < 0 else report.oos_legs
    round_trips_ok = report.position_rows == 0 or (
        report.oos_round_trips >= MIN_ROUND_TRIPS
    )
    return GateResult(
        pnl_positive=report.oos_net_pnl_usd > thresholds.min_oos_net_pnl_usd,
        min_trades=legs >= thresholds.min_trades and round_trips_ok,
        min_days=report.oos_trading_days >= thresholds.min_trading_days,
        max_drawdown=report.oos_max_drawdown_usd <= thresholds.max_drawdown_usd,
        bounds=(
            report.max_order_notional_usd <= thresholds.max_order_notional_usd
            and report.max_symbol_notional_usd <= thresholds.max_symbol_notional_usd
            and report.max_total_notional_usd <= thresholds.max_total_notional_usd
        ),
    )


def report_path_for(ruleset_path: pathlib.Path) -> pathlib.Path:
    """``R.json`` -> ``R.report.json`` next to the ruleset (§6)."""
    return ruleset_path.with_suffix(REPORT_SUFFIX)


def write_report(
    ruleset_path: pathlib.Path,
    full_hash: str,
    harness: HarnessReport,
    gates: GateResult,
    thresholds: GateThresholds,
) -> pathlib.Path:
    """The worker report — the artifact stage-ruleset later binds on
    (hash + schema version + gates.all_passed). Written on pass AND fail."""
    path = report_path_for(ruleset_path)
    payload = {
        "schema_version": REPORT_SCHEMA_VERSION,
        "ruleset_hash": full_hash,
        "generated_ts": int(time.time()),
        "split": harness.split,
        "oos": {
            "net_pnl_usd": harness.oos_net_pnl_usd,
            "trades": harness.oos_trades,
            "trading_days": harness.oos_trading_days,
            "max_drawdown_usd": harness.oos_max_drawdown_usd,
            # VM2 V5 (D-3) additive keys — absent-tolerant readers.
            "round_trips": harness.oos_round_trips,
            "legs": harness.oos_trades if harness.oos_legs < 0 else harness.oos_legs,
        },
        "bounds": {
            "max_order_notional_usd": harness.max_order_notional_usd,
            "max_symbol_notional_usd": harness.max_symbol_notional_usd,
            "max_total_notional_usd": harness.max_total_notional_usd,
        },
        "gates": {
            "pnl_positive": gates.pnl_positive,
            "min_trades": gates.min_trades,
            "min_days": gates.min_days,
            "max_drawdown": gates.max_drawdown,
            "bounds": gates.bounds,
            "all_passed": gates.all_passed,
        },
        "thresholds": dataclasses.asdict(thresholds),
        # VM2 V5 (D-3): position-ruleset context for report readers.
        "position_rows": harness.position_rows,
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True))
    return path


# ---- stage/commit gate binding (§6; the ONLY path to Stage/Commit frames)


def check_stage_binding(ruleset_path: pathlib.Path, report_path: pathlib.Path) -> tuple[str, bytes]:
    """The §6 GATE BINDING SITE: recompute sha256 over the ruleset file,
    require the worker report to (a) exist, (b) carry our schema version,
    (c) carry exactly that hash, (d) say ``gates.all_passed``. Any
    violation is [`GateRefused`] (exit 3 — §11 lists report-missing here
    too). Returns ``(full_hash_hex, hash128)`` on success."""
    full_hash, hash128 = ruleset_hashes(ruleset_path)
    if not report_path.is_file():
        raise GateRefused(f"report missing: {report_path}")
    try:
        raw = json.loads(report_path.read_text())
    except ValueError as exc:
        raise GateRefused(f"report is not JSON: {report_path}: {exc}") from exc
    if not isinstance(raw, dict):
        raise GateRefused(f"report is not a JSON object: {report_path}")
    obj = typing.cast(dict[str, object], raw)
    version = obj.get("schema_version")
    if version != REPORT_SCHEMA_VERSION:
        raise GateRefused(f"report schema_version {version!r} != {REPORT_SCHEMA_VERSION}")
    if obj.get("ruleset_hash") != full_hash:
        raise GateRefused("report ruleset_hash does not match the ruleset file (recomputed)")
    gates = obj.get("gates")
    if (
        not isinstance(gates, dict)
        or typing.cast(dict[str, object], gates).get("all_passed") is not True
    ):
        raise GateRefused("report gates.all_passed is not true")
    return full_hash, hash128


def hash128_wire(hash128: bytes) -> tuple[int, int]:
    """hash128 -> (px, qty) i64 halves, little-endian signed (§13
    decision 5; byte convention pinned by the shared golden vectors)."""
    if len(hash128) != HASH128_LEN:
        raise ValueError(f"hash128 must be {HASH128_LEN} bytes, got {len(hash128)}")
    px = int.from_bytes(hash128[0:8], "little", signed=True)
    qty = int.from_bytes(hash128[8:16], "little", signed=True)
    return px, qty


def _send_ruleset_frame(client: claude_worker.uds.UdsClient, kind: int, hash128: bytes) -> int:
    px, qty = hash128_wire(hash128)
    return client.send_cmd(
        sym=claude_worker.frames.SYMBOL_ID_NONE,
        px=px,
        qty=qty,
        ttl_ns=0,
        kind=kind,
        venue=claude_worker.frames.VENUE_AI,
        strategy_id=claude_worker.frames.STRATEGY_SLOT_VM,
        side=claude_worker.frames.SIDE_NONE,
        param_id=0,
        flags=0,
    )


def stage_ruleset(
    state: claude_worker.state.State,
    client: claude_worker.uds.UdsClient,
    ruleset_path: pathlib.Path,
    report_path: pathlib.Path,
    author_mode: str,
) -> tuple[int, str]:
    """Bind gates -> record the registry row -> send RulesetStage{hash128}
    (§6 order: record, then send). ``client`` must be connected with the
    heartbeat already sent. Both modes route here — serve's commander path
    (8h strategist) calls this same function, so gates bind in code
    everywhere. Returns ``(seq, full_hash_hex)``."""
    full_hash, hash128 = check_stage_binding(ruleset_path, report_path)
    state.stage_ruleset(full_hash, str(ruleset_path), str(report_path), author_mode)
    seq = _send_ruleset_frame(client, claude_worker.frames.KIND_RULESET_STAGE, hash128)
    return seq, full_hash


def commit_ruleset(
    state: claude_worker.state.State,
    client: claude_worker.uds.UdsClient,
    full_hash: str,
) -> int:
    """Require a STAGED, gates-passed registry row for ``full_hash``; send
    RulesetCommit{hash128}; stamp ``committed_ts`` (send-then-record: a
    failed send leaves no phantom commit). Unknown/unstaged/failed hash ⇒
    [`GateRefused`] (exit 3). Returns the seq used."""
    row = state.ruleset_row(full_hash)
    if row is None:
        raise GateRefused(f"no staged ruleset for hash {full_hash}")
    _hash, _path, _report, gates_passed, _mode, staged_ts, _committed = row
    if staged_ts is None or not gates_passed:
        raise GateRefused(f"ruleset {full_hash} is not a staged, gates-passed row")
    hash128 = bytes.fromhex(full_hash)[:HASH128_LEN]
    seq = _send_ruleset_frame(client, claude_worker.frames.KIND_RULESET_COMMIT, hash128)
    state.mark_ruleset_committed(full_hash)
    return seq


def default_run_fn(argv: list[str]) -> str:
    """Real subprocess seam (binary ships in 8h). Nonzero exit or a
    missing binary is a BacktestError carrying a bounded stderr tail."""
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    except OSError as exc:
        raise BacktestError(f"harness spawn failed: {exc}") from exc
    if proc.returncode != 0:
        tail = proc.stderr[-500:] if proc.stderr else "<no stderr>"
        raise BacktestError(f"harness exit {proc.returncode}: {tail}")
    return proc.stdout


def run_backtest(
    ruleset_path: pathlib.Path,
    replay_dir: pathlib.Path,
    split: str = "70/30",
    thresholds: GateThresholds | None = None,
    run_fn: typing.Callable[[list[str]], str] | None = None,
) -> BacktestOutcome:
    """Drive the harness, validate its report, evaluate gates, write the
    worker report next to the ruleset. Gate FAIL is a normal outcome
    (report written, ``all_passed`` False — verb maps it to exit 3);
    BacktestError is for reports that cannot be trusted at all."""
    gate_cfg = GateThresholds() if thresholds is None else thresholds
    runner = default_run_fn if run_fn is None else run_fn
    full_hash, _hash128 = ruleset_hashes(ruleset_path)
    argv = [
        ENGINE_BINARY,
        "backtest",
        "--ruleset",
        str(ruleset_path),
        "--replay-dir",
        str(replay_dir),
        "--split",
        split,
    ]
    harness = parse_harness_report(runner(argv), full_hash)
    gates = evaluate_gates(harness, gate_cfg)
    report_path = write_report(ruleset_path, full_hash, harness, gates, gate_cfg)
    return BacktestOutcome(
        all_passed=gates.all_passed,
        report_path=report_path,
        gates=gates,
        harness=harness,
    )
