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

# The engine binary name (PATH-resolved by default; absolute in prod .env
# wiring arrives with the 8h harness).
ENGINE_BINARY: str = "multivenue-engine"

# Version of BOTH the harness stdout contract and the worker report file.
# stage-ruleset (item 12) refuses any other version.
REPORT_SCHEMA_VERSION: int = 1

REPORT_SUFFIX: str = ".report.json"

HASH128_LEN: int = 16


class BacktestError(Exception):
    """Harness failure or contract violation: bad exit, garbage stdout,
    schema/hash mismatch. Fail-fast — a report we cannot trust must never
    reach the gates."""


@dataclasses.dataclass(frozen=True, slots=True)
class GateThresholds:
    """§5.1 gate numbers. Defaults pin the design text + risk-policy
    caps; operators tighten (never loosen past risk-policy) via worker
    config wiring in item 12."""

    min_oos_net_pnl_usd: float = 0.0  # strict >
    min_trades: int = 50
    min_trading_days: int = 2
    max_drawdown_usd: float = 200.0  # risk-policy daily realized-loss line
    max_order_notional_usd: float = 100.0
    max_symbol_notional_usd: float = 250.0
    max_total_notional_usd: float = 1_000.0


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
    """Validated machine-readable harness output (stdout contract)."""

    ruleset_hash: str
    split: str
    oos_net_pnl_usd: float
    oos_trades: int
    oos_trading_days: int
    oos_max_drawdown_usd: float
    max_order_notional_usd: float
    max_symbol_notional_usd: float
    max_total_notional_usd: float


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
    )


def evaluate_gates(report: HarnessReport, thresholds: GateThresholds) -> GateResult:
    """The §5.1 gate matrix — pure code, no prompts, no overrides."""
    return GateResult(
        pnl_positive=report.oos_net_pnl_usd > thresholds.min_oos_net_pnl_usd,
        min_trades=report.oos_trades >= thresholds.min_trades,
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
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True))
    return path


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
