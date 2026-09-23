# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""hyparb_ladder — the HYPARB correction ladder for the nightly report (plan §10).

The slot-0 member carries four corrections the backtest was wrong about,
each a knob in ``hyparb.toml`` (plan §8.4). The ladder replays one
bounded unit through ``multivenue-engine backtest --member hyparb`` once
per RUNG, turning the corrections on one at a time:

- ``r0`` — none: no depth cap, no basis control, no gas, a 1 ns hedge
  latency (the naive model);
- ``r1`` — + the depth cap (the single largest correction, plan §8.4.1);
- ``r2`` — + the artifact's own hedge latency;
- ``r3`` — the artifact exactly as the live member runs it (+ basis
  control and the gas charged per attempt).

``r3`` is the number to hold the live paper to; the rungs below it say
how much of the naive figure each correction takes away. The nightly
``pnl_report`` renders the ladder beside slot 0's live paper row
(``--hyparb-ladder``; off by default — four replays per unit).

A rung is the artifact with its ``[hyparb]`` keys REWRITTEN — never a
second grammar: the engine parses every rung with the one parser, so a
rung that does not parse is a failed rung, not a silently different one.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import re
import typing

import claude_worker.backtest

RunFn = typing.Callable[[list[str]], tuple[int, str, str]]

#: (rung, the ``[hyparb]`` keys it rewrites). Cumulative by construction:
#: each rung rewrites fewer keys than the one before it.
RUNGS: tuple[tuple[str, dict[str, str]], ...] = (
    (
        "r0",
        {"depth_cap_enabled": "0", "basis_enabled": "0", "gas_p50_usd_1e6": "0", "lag_ns": "1"},
    ),
    ("r1", {"basis_enabled": "0", "gas_p50_usd_1e6": "0", "lag_ns": "1"}),
    ("r2", {"basis_enabled": "0", "gas_p50_usd_1e6": "0"}),
    ("r3", {}),
)

#: The member's counters line on the backtest's stderr.
_COUNTERS_PREFIX = "member: hyparb pool_events="

#: Integer / dollar fields the ladder keeps from the counters line.
_INT_FIELDS = ("arbs", "side_buy", "side_sell", "size_capped", "hedges_perp", "hedges_spot",
               "hedges_missed", "amm_judged_fills", "amm_canceled")
_USD_FIELDS = ("gas_usd", "pnl_predicted_usd", "funding_earned_usd")


class LadderError(Exception):
    """A rung could not be built (the artifact lacks a key it rewrites)."""


def rung_artifact(text: str, overrides: dict[str, str]) -> str:
    """``text`` with each ``key = value`` line of ``overrides`` rewritten
    in place (comments and every other byte kept). Every key must be
    present exactly once — a rung that silently kept the operator's value
    would be a different rung than its name says."""
    out = text
    for key, value in overrides.items():
        pattern = re.compile(rf"^(\s*{re.escape(key)}\s*=\s*)([^#\n]*?)(\s*(#.*)?)$", re.MULTILINE)
        found = pattern.findall(out)
        if len(found) != 1:
            raise LadderError(f"hyparb.toml: `{key}` must appear exactly once (found {len(found)})")
        out = pattern.sub(lambda m: f"{m.group(1)}{value}{m.group(3)}", out, count=1)
    return out


def _parse_counters(stderr: str) -> dict[str, object]:
    for line in stderr.splitlines():
        if line.startswith(_COUNTERS_PREFIX):
            toks = line[len("member: hyparb ") :].split()
            fields = dict(tok.split("=", 1) for tok in toks if "=" in tok)
            row: dict[str, object] = {}
            for k in _INT_FIELDS:
                row[k] = int(fields.get(k, "0"))
            for k in _USD_FIELDS:
                row[k] = float(fields.get(k, "0"))
            return row
    raise ValueError("no `member: hyparb` counters line on stderr")


def run_rung(
    unit_dir: pathlib.Path,
    artifact: pathlib.Path,
    fee_flags: list[str],
    run_fn: RunFn,
) -> dict[str, object]:
    """One replay of ``unit_dir`` with ``artifact``: OOS net after gas,
    trades and the member's counters. Raises on a failed or unreadable
    replay (the caller records the rung as failed)."""
    argv = [
        claude_worker.backtest.ENGINE_BINARY,
        "backtest",
        "--member",
        "hyparb",
        "--hyparb",
        str(artifact),
        "--replay-dir",
        str(unit_dir),
        "--split",
        "0/100",
        *fee_flags,
    ]
    code, out, err = run_fn(argv)
    if code != 0:
        raise ValueError(f"backtest exited {code}: {err.strip()[-300:]}")
    obj = json.loads(out.strip())
    if obj.get("schema_version") != 1:
        raise ValueError("backtest stdout is not schema-1")
    oos = obj.get("oos") or {}
    row: dict[str, object] = {
        "oos_net_usd": float(oos.get("net_pnl_usd", 0.0)),
        "trades": int(oos.get("trades", 0)),
    }
    row.update(_parse_counters(err))
    return row


def run_ladder(
    unit_dir: pathlib.Path,
    artifact_text: str,
    scratch: pathlib.Path,
    fee_flags: list[str],
    run_fn: RunFn,
) -> list[dict[str, object]]:
    """Every rung over one unit; a failed rung is a row with ``error``."""
    scratch.mkdir(parents=True, exist_ok=True)
    rows: list[dict[str, object]] = []
    for name, overrides in RUNGS:
        path = scratch / f"hyparb-{name}.toml"
        try:
            path.write_text(rung_artifact(artifact_text, overrides), encoding="utf-8")
            row = run_rung(unit_dir, path, fee_flags, run_fn)
        except (LadderError, ValueError, OSError) as exc:
            rows.append({"rung": name, "error": str(exc)})
            continue
        rows.append({"rung": name, **row})
    return rows


def merge_ladder(per_unit: list[list[dict[str, object]]]) -> list[dict[str, object]]:
    """Sum each rung across the day's units (dollars rendered to 1e-6,
    as the audit merge does). A rung that failed in any unit is marked
    — a partial sum would read as a smaller number, not a missing one."""
    merged: list[dict[str, object]] = []
    for name, _ in RUNGS:
        acc: dict[str, object] = {"rung": name, "units": 0, "failed_units": 0,
                                  "oos_net_usd": 0.0, "trades": 0}
        for k in _INT_FIELDS:
            acc[k] = 0
        for k in _USD_FIELDS:
            acc[k] = 0.0
        for rows in per_unit:
            row = next((r for r in rows if r.get("rung") == name), None)
            if row is None or "error" in row:
                acc["failed_units"] = int(acc["failed_units"]) + 1
                continue
            acc["units"] = int(acc["units"]) + 1
            acc["oos_net_usd"] = float(acc["oos_net_usd"]) + float(row["oos_net_usd"])
            acc["trades"] = int(acc["trades"]) + int(row["trades"])
            for k in _INT_FIELDS:
                acc[k] = int(acc[k]) + int(row[k])
            for k in _USD_FIELDS:
                acc[k] = float(acc[k]) + float(row[k])
        for k in ("oos_net_usd", *_USD_FIELDS):
            acc[k] = f"{float(acc[k]):.6f}"
        merged.append(acc)
    return merged


def summary_lines(section: dict[str, object]) -> list[str]:
    """The summary's hyparb block: the live paper row(s), then one line
    per rung — the side balance on each, because a persistent skew with
    basis control ON is the tell that it is wrong (plan §10 #2)."""
    lines: list[str] = []
    paper = section.get("paper") or []
    if not paper:
        lines.append("hyparb: live paper: no slot-0 row this day")
    for row in paper:
        lines.append(
            f"hyparb: live {row.get('accounting', '?')}: fills={row.get('fills', 0)} "
            f"trades={row.get('trades', 0)} net={row.get('net_usd', '0')} "
            f"fees={row.get('fees_usd', '0')}"
        )
    for r in section.get("ladder") or []:
        failed = f" failed_units={r['failed_units']}" if r.get("failed_units") else ""
        lines.append(
            f"hyparb: ladder {r['rung']}: units={r['units']}{failed} "
            f"net_after_gas={r['oos_net_usd']} "
            f"trades={r['trades']} arbs={r['arbs']} buy/sell={r['side_buy']}/{r['side_sell']} "
            f"size_capped={r['size_capped']} perp/spot={r['hedges_perp']}/{r['hedges_spot']} "
            f"missed={r['hedges_missed']} gas={r['gas_usd']} predicted={r['pnl_predicted_usd']} "
            f"funding={r['funding_earned_usd']}"
        )
    return lines
