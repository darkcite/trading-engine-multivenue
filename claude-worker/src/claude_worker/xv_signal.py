# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""xv_signal — position-aware cross-venue reversion (M5 session-2 port).

The research-loop hour (2026-08-29) proved the edge is real (+$409 OOS
at honest thresholds) but the position-blind ruleset VM cannot hold it
inside the $20k/sym cap — persistent basis regimes accumulate. This
module is the operator-approved carrier: a 5-minute cron with NET
POSITION TRACKING, hard-capped at ONE open pair position per pair.

A standalone MODULE (``python -m claude_worker.xv_signal``) — NOT a
verb. One cycle, NO network I/O:

1. Read CURRENT mids from the LIVE capture's tick-file TAILS (the
   newest run dir; last ``TAIL_BYTES`` of each venue file). Two
   staleness guards, both required because capture ``ts_ns`` is the
   engine's MONOTONIC clock (not wall time): (a) GLOBAL — the newest
   tick file's mtime must be within ``MAX_MID_AGE_S`` of wall now (a
   stopped engine never trades); (b) RELATIVE — each pair leg's tick
   must be within ``MAX_MID_AGE_S`` of the newest tick observed in
   this cycle (a single quiet leg never trades on a fossil mid).
2. Per pair (zero-centered pairs only, from the session-2 mining):
   dev_bps = (mid_sym − mid_ref)/mid_ref × 1e4.
   ENTER when |dev| ≥ ``ENTRY_BPS`` and flat: SELL the rich leg, BUY
   the cheap leg — a hedged pair, ``LEG_NOTIONAL_USD`` per leg
   (operator rulings 2026-08-29: $50k book, $10k/position; both legs
   under the $10k/order research-tier cap).
   EXIT when |dev| ≤ ``EXIT_BPS`` or the sign flips: close both legs.
3. Emit batch/push.sh/digest under ``~/multivenue/worker/xv/`` and
   the rolling ``state.json`` — same contract as carry_signal (whose
   Intent/crossing/render helpers this module reuses).

Cadence honesty: a 5-min cron harvests the PERSISTENT-dislocation
component only (regimes lasting ≥ minutes-to-hours — exactly what the
mining showed persists); the 5-second oscillation P&L of the v1
backtest is NOT claimable here and is not claimed.

Regime gate (RG5, plan §5.1): ``REGIME_LABEL`` (the §3.3 term grammar,
default empty = ANY) gates ENTRIES only through
``claude_worker.regime.lane_gate`` — exits are never gated, a held
position drains by its own law. The words come from the engine's
``/metrics`` when reachable, else the fresh ``declared.json``, else
UNKNOWN (a constrained profile then fails closed).
"""

import argparse
import dataclasses
import datetime
import glob
import json
import os
import pathlib
import struct
import sys
import time

import claude_worker.carry_signal
import claude_worker.regime

# (sym, ref, sym_descriptor, ref_descriptor, tag) — zero-centered
# pairs per the 2026-08-29 mining (bn↔okx med −0.2 bps, bn↔hl +0.1;
# the deribit pair is EXCLUDED: −6 bps standing basis).
PAIRS: tuple[tuple[int, int, str, str, str], ...] = (
    (33554433, 7, "okx:BTC-USDT", "binance:btcusdt", "xv-okx-bnspot"),
    (67108865, 16777729, "hyperliquid:BTC", "binance-usdm:btcusdt", "xv-hl-bnusdm"),
)

ENTRY_BPS: float = 4.0
EXIT_BPS: float = 1.0
LEG_NOTIONAL_USD: float = 9_900.0
MAX_MID_AGE_S: float = 120.0
TAIL_BYTES: int = 512 * 1024
SLOT: int = 64
#: RG5 entry label (§3.3 terms; empty = ANY, the pre-RG5 behaviour).
REGIME_LABEL: tuple[str, ...] = ()


def newest_run_dir(logs_dir: pathlib.Path) -> pathlib.Path | None:
    runs = sorted(p for p in logs_dir.glob("run-*") if p.is_dir())
    return runs[-1] if runs else None


def tail_mids(run_dir: pathlib.Path, want: set[int]) -> dict[int, tuple[float, float, float]]:
    """sym → (bid, ask, ts_ns) from the newest ticks in each venue
    file's tail. Zero-copy-ish: one bounded read per file, newest
    slot per sym wins."""
    out: dict[int, tuple[float, float, float]] = {}
    for f in run_dir.glob("*-ticks.pmlr"):
        try:
            size = f.stat().st_size
            with open(f, "rb") as fh:
                start = max(64, size - TAIL_BYTES)
                # Align to slot grid (header is 64 B, slots are 64 B).
                start -= (start - 64) % SLOT
                fh.seek(start)
                data = fh.read()
        except OSError:
            continue
        n = len(data) // SLOT
        for i in range(n):
            off = i * SLOT
            ts, sym = struct.unpack_from("<QI", data, off)
            if sym not in want:
                continue
            bid, = struct.unpack_from("<q", data, off + 16)
            ask, = struct.unpack_from("<q", data, off + 32)
            if bid <= 0 or ask <= 0:
                continue
            prev = out.get(sym)
            if prev is None or ts > prev[2]:
                out[sym] = (bid / 1e6, ask / 1e6, ts)
    return out


def load_state(path: pathlib.Path) -> dict:
    try:
        d = json.loads(path.read_text())
    except (OSError, ValueError):
        return {"positions": {}}
    if not isinstance(d, dict) or not isinstance(d.get("positions"), dict):
        return {"positions": {}}
    return d


def pair_legs_for(
    tag: str,
    rich_desc: str,
    cheap_desc: str,
    market_map: dict[str, int],
    marks: dict[int, tuple[float, float]],
) -> tuple[list, list[str]]:
    """Sell the rich leg / buy the cheap leg via the carry helpers."""
    return claude_worker.carry_signal.explicit_leg_intents(
        tag, [(rich_desc, "ask"), (cheap_desc, "bid")], market_map, marks
    )


def run_cycle(
    logs_dir: pathlib.Path,
    map_path: pathlib.Path,
    out_dir: pathlib.Path,
    now_ns: int | None = None,
    regime_words: dict[str, int] | None = None,
) -> pathlib.Path:
    now = time.time_ns() if now_ns is None else now_ns
    run_dir = newest_run_dir(logs_dir)
    market_map = claude_worker.carry_signal.load_map(map_path)
    lines: list[str] = []
    intents: list = []
    notes: list[str] = []
    out_dir.mkdir(parents=True, exist_ok=True)
    state_path = out_dir / "state.json"
    state = load_state(state_path)
    positions: dict = state["positions"]
    entries_open, regime_tell = claude_worker.regime.lane_gate(
        REGIME_LABEL, now // 1_000_000, regime_words
    )
    if REGIME_LABEL:
        lines.append(regime_tell)

    mids: dict[int, tuple[float, float, float]] = {}
    feed_live = False
    if run_dir is None:
        lines.append("no run dir — no mids, holding")
    else:
        want = {s for p in PAIRS for s in (p[0], p[1])}
        mids = tail_mids(run_dir, want)
        # Global guard (wall clock vs file mtimes — see module docs).
        try:
            newest_mtime = max(
                f.stat().st_mtime for f in run_dir.glob("*-ticks.pmlr")
            )
            # Wall clock deliberately (not the injectable now): the
            # question is whether the ENGINE is writing right now.
            feed_live = (time.time() - newest_mtime) <= MAX_MID_AGE_S
        except (ValueError, OSError):
            feed_live = False
        if not feed_live:
            lines.append("capture feed stale (mtime guard) — holding")

    # marks for the crossing-px helper: (bid, ask) floats.
    marks = {s: (m[0], m[1]) for s, m in mids.items()}

    for sym, ref, sym_desc, ref_desc, tag in PAIRS:
        ms = mids.get(sym)
        mr = mids.get(ref)
        pos = positions.get(tag)
        if not feed_live:
            continue
        if ms is None or mr is None:
            lines.append(f"  {tag:14s} missing mids ({'sym' if ms is None else ''}{'ref' if mr is None else ''}) — hold")
            continue
        # Relative guard: monotonic ts vs the newest tick this cycle.
        newest_ts = max(m[2] for m in mids.values())
        lag_s = (newest_ts - min(ms[2], mr[2])) / 1e9
        if lag_s > MAX_MID_AGE_S:
            lines.append(f"  {tag:14s} leg lags feed by {lag_s:.0f}s — hold")
            continue
        mid_s = (ms[0] + ms[1]) / 2
        mid_r = (mr[0] + mr[1]) / 2
        dev_bps = (mid_s - mid_r) / mid_r * 1e4

        if pos is None:
            if abs(dev_bps) >= ENTRY_BPS and not entries_open:
                lines.append(f"  {tag:14s} dev={dev_bps:+.1f}bps ENTRY-BLOCKED: regime")
            elif abs(dev_bps) >= ENTRY_BPS:
                # sym rich ⇒ sell sym / buy ref; sym cheap ⇒ reverse.
                rich, cheap = (sym_desc, ref_desc) if dev_bps > 0 else (ref_desc, sym_desc)
                legs, skips = pair_legs_for(f"{tag}-entry", rich, cheap, market_map, marks)
                if skips:
                    notes.extend(skips)
                    lines.append(f"  {tag:14s} dev={dev_bps:+.1f}bps ENTRY-BLOCKED: {'; '.join(skips)}")
                    continue
                intents.extend(legs)
                positions[tag] = {
                    "entered_ns": now,
                    "entry_dev_bps": dev_bps,
                    "short_desc": rich,
                    "long_desc": cheap,
                }
                lines.append(f"  {tag:14s} dev={dev_bps:+.1f}bps ENTER short={rich} long={cheap}")
            else:
                lines.append(f"  {tag:14s} dev={dev_bps:+.1f}bps flat")
        else:
            entry_dev = pos["entry_dev_bps"]
            reverted = abs(dev_bps) <= EXIT_BPS or (dev_bps * entry_dev) < 0
            if reverted:
                legs, skips = claude_worker.carry_signal.explicit_leg_intents(
                    f"{tag}-exit",
                    [(pos["short_desc"], "bid"), (pos["long_desc"], "ask")],
                    market_map,
                    marks,
                )
                if skips:
                    notes.extend(skips)
                    lines.append(f"  {tag:14s} dev={dev_bps:+.1f}bps EXIT-BLOCKED: {'; '.join(skips)}")
                    continue
                intents.extend(legs)
                del positions[tag]
                age_h = (now - pos["entered_ns"]) / 3.6e12
                lines.append(
                    f"  {tag:14s} dev={dev_bps:+.1f}bps EXIT (entry {entry_dev:+.1f}, held {age_h:.1f}h)"
                )
            else:
                lines.append(f"  {tag:14s} dev={dev_bps:+.1f}bps HELD (entry {entry_dev:+.1f})")

    stamp = datetime.datetime.fromtimestamp(now / 1e9, tz=datetime.timezone.utc).strftime(
        "%Y%m%dT%H%M%SZ"
    )
    batch = {
        "schema": 1,
        "generated_ns": now,
        "intents": [dataclasses.asdict(i) for i in intents],
    }
    (out_dir / f"batch-{stamp}.json").write_text(json.dumps(batch, indent=1, sort_keys=True) + "\n")
    (out_dir / "push.sh").write_text(claude_worker.carry_signal.render_push_sh(intents))
    digest = out_dir / f"digest-{stamp}.txt"
    body = [
        f"xv_signal digest {stamp} (entry ≥{ENTRY_BPS}bps, exit ≤{EXIT_BPS}bps, ${LEG_NOTIONAL_USD:.0f}/leg, 1 position/pair)",
        *lines,
        f"intents: {len(intents)}" + (" — push.sh ready" if intents else " — nothing to push"),
    ]
    if notes:
        body.append("skips: " + "; ".join(sorted(set(notes))))
    digest.write_text("\n".join(body) + "\n")
    state_path.write_text(json.dumps(state, indent=1, sort_keys=True) + "\n")
    return digest


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="claude_worker.xv_signal")
    ap.add_argument("--logs", default=os.environ.get(
        "CLAUDE_WORKER_REPLAY_DIR", "~/multivenue/logs"))
    ap.add_argument("--market-map", default=os.environ.get(
        "CLAUDE_WORKER_MARKET_MAP", "~/multivenue/worker/market-map.json"))
    ap.add_argument("--out", default="~/multivenue/worker/xv")
    args = ap.parse_args(argv)
    digest = run_cycle(
        pathlib.Path(os.path.expanduser(args.logs)),
        pathlib.Path(os.path.expanduser(args.market_map)),
        pathlib.Path(os.path.expanduser(args.out)),
    )
    sys.stdout.write(digest.read_text())
    sys.stdout.write(f"[digest] {digest}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
