"""Shared 8h-H5 test builders: crafted PMLR tick files (known spans for
the §8.3 window math) and committed-registry seeding. NOT collected by
pytest (no ``test_`` prefix); imported by the monitor/research suites.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib

import claude_worker.backtest
import claude_worker.pmlr
import claude_worker.state

_SLOT = claude_worker.pmlr.SLOT_SIZE
_HDR = claude_worker.pmlr.HEADER_SIZE


def write_ticks(
    path: pathlib.Path,
    ts_list: list[int],
    epoch_ns: int,
    sym: int = 42,
    venue: int = 0,
) -> None:
    """One v2 PMLR tick file with the given slot timestamps (the reader's
    own struct formats — no second layout to drift)."""
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately
        claude_worker.pmlr.MAGIC, 2, claude_worker.pmlr.SLOT_KIND_TICK, epoch_ns
    )
    blob = bytearray(header + bytes(_HDR - len(header)))
    for i, ts in enumerate(ts_list):
        slot = claude_worker.pmlr._TICK.pack(  # noqa: SLF001
            ts, sym, i + 1, 400_000, 1_000_000, 420_000, 1_000_000, venue
        )
        blob.extend(slot + bytes(_SLOT - len(slot)))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(blob))


def write_run(
    replay_dir: pathlib.Path,
    epoch_ns: int,
    ts_list: list[int],
    venue_label: str = "pm",
) -> pathlib.Path:
    """One ``run-<epoch>`` dir holding a single crafted tick file."""
    run_dir = replay_dir / f"run-{epoch_ns}"
    run_dir.mkdir(parents=True, exist_ok=True)
    if ts_list:
        write_ticks(run_dir / f"{venue_label}-ticks.pmlr", ts_list, epoch_ns)
    return run_dir


def seed_committed_ruleset(
    state: claude_worker.state.State,
    tmp_path: pathlib.Path,
    name: str,
    row_name: str,
    staged_ts: int,
    committed_ts: int,
    model: str | None = None,
    thesis: str | None = None,
) -> tuple[str, pathlib.Path, pathlib.Path]:
    """A gates-passed COMMITTED registry row backed by REAL files (the
    frozen restage path re-reads both): a canonical one-row artifact and
    a passing worker report written by the frozen ``write_report``.
    Returns ``(full_hash, ruleset_path, report_path)``."""
    ruleset_path = tmp_path / f"{name}.json"
    artifact = {
        "rows": [
            {
                "name": row_name,
                "family": "crypto",
                "trigger": {"type": "level_breach", "level": 0.42},
                "sym": 42,
                "side": "bid",
                "edge_bps": 80,
                "horizon_ms": 1500,
                "max_risk_usd": 50.0,
            }
        ]
    }
    ruleset_path.write_bytes(json.dumps(artifact, separators=(",", ":")).encode())
    full_hash, _hash128 = claude_worker.backtest.ruleset_hashes(ruleset_path)
    harness = claude_worker.backtest.HarnessReport(
        ruleset_hash=full_hash,
        split="70/30",
        oos_net_pnl_usd=5.0,
        oos_trades=60,
        oos_trading_days=3,
        oos_max_drawdown_usd=20.0,
        max_order_notional_usd=50.0,
        max_symbol_notional_usd=96.8,
        max_total_notional_usd=96.8,
    )
    thresholds = claude_worker.backtest.GateThresholds()
    gates = claude_worker.backtest.evaluate_gates(harness, thresholds)
    assert gates.all_passed
    report_path = claude_worker.backtest.write_report(
        ruleset_path, full_hash, harness, gates, thresholds
    )
    state.stage_ruleset(
        full_hash,
        str(ruleset_path),
        str(report_path),
        "auto",
        staged_ts,
        model=model,
        thesis=thesis,
    )
    state.mark_ruleset_committed(full_hash, ts=committed_ts)
    return full_hash, ruleset_path, report_path
