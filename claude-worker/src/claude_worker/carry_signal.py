# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""carry_signal — the M5 external-strategies signal lane (CVFC-1 + S1 pilot).

A standalone MODULE (``python -m claude_worker.carry_signal``) — NOT a
verb (frozen CLI surface; the candles/iv_digest/refdata/funding
precedent). One cycle:

1. Read funding history from the ``funding`` table BESIDE candles in
   ``candles.db`` (the WS11 lane owns fetching — run
   ``python -m claude_worker.funding`` first; this module performs NO
   network I/O at all).
2. Compute per-(venue, coin) annualized funding APRs with the
   per-venue cadence law (below), then:
   - **CVFC-1** (``CVFC-1-cross-venue-funding-carry.md`` §2): ordered
     venue pairs scored per coin; enter when
     ``mean_24h(APR_short) − mean_24h(APR_long) ≥ 20 APR points``;
     exit when the HELD pair's spread < 0 after ≥ 96 h; ≤ 5 positions;
     majors (BTC/ETH) computed but EXCLUDED from entry by the doc's
     own economics.
   - **S1 pilot** (Consolidated §5.1, operator-ruled FIXED name set):
     Binance↔Bybit |spread| with the 50%/30% entry confirms; exits
     at directional spread < 10% or age > 10 d. EXECUTABLE since the
     operator's 2026-08-29 bybit venue-table unfreeze (D1 pattern) —
     long the more-negative-funding venue, short the other, both
     legs as paper intents.
3. Emit, under ``~/multivenue/worker/carry/``: the intent batch JSON,
   a ``push.sh`` of exact ``claude-worker push`` verb lines (the
   session reviews then executes it — frames stay verb-built), a
   human digest, and the rolling position state.

Cadence law (R4 lesson 9 — units): the ``funding.rate`` column stores
RAW per-print rates. Daily return = Σ(prints in the window), EXCEPT
Deribit, whose rows are HOURLY SAMPLES of an 8-hour rolling
``interest_8h`` — Σ over-counts 8×, so Deribit sums divide by 8.
APR = daily × 365.

Paper-fill law: intent prices CROSS the captured book (bid at
last_ask × (1+slip), ask at last_bid × (1−slip)) so audit-pnl's
strict-cross fill model — the strategies' own 0%-maker FLOOR
scenario — can fill them. Leg notional sits under the $10k/order
research-tier cap (operator ruling 2026-08-29, $50k book).
"""

import argparse
import dataclasses
import datetime
import json
import os
import pathlib
import sqlite3
import sys
import time
import typing

CVFC_COINS: tuple[str, ...] = ("BTC", "ETH", "SOL", "XRP", "DOGE", "ADA", "LTC")
CVFC_MAJORS_EXCLUDED: tuple[str, ...] = ("BTC", "ETH")
S1_PILOT: tuple[str, ...] = (
    "COTIUSDT",
    "DEXEUSDT",
    "BANKUSDT",
    "ERAUSDT",
    "BLESSUSDT",
    "1000RATSUSDT",
    "UAIUSDT",
)

ENTRY_SPREAD_APR: float = 0.20  # CVFC: 20 APR points
EXIT_MIN_HOLD_H: float = 96.0
MAX_POSITIONS: int = 5
S1_ENTRY_ABS_APR: float = 0.50
S1_CONFIRM_3D_APR: float = 0.30
# Operator rulings 2026-08-29: strategies assume a $50k book; CVFC-1
# spec legs are $10k (under the $10k/order research-tier cap exactly).
LEG_NOTIONAL_USD: float = 9_900.0
INTENT_TTL_S: float = 3600.0
CROSS_SLIP: float = 0.01

# Descriptor prefixes whose venue is addressable by the AiCmd wire.
INTENT_VENUE_BY_PREFIX: dict[str, str] = {
    "hyperliquid": "hyperliquid",
    "deribit": "deribit",
    "binance-usdm": "binance",
    # xv_signal pairs (M5 session 2): spot legs + okx.
    "binance": "binance",
    "okx": "okx",
    # Operator ruling 2026-08-29: the bybit venue-table unfreeze.
    "bybit-linear": "bybit",
    "bybit": "bybit",
}

S1_MAX_POSITIONS: int = 4
S1_EXIT_DIRECTIONAL_APR: float = 0.10
S1_MAX_AGE_H: float = 240.0

WINDOW_24H_MS: int = 24 * 3600 * 1000
WINDOW_3D_MS: int = 3 * WINDOW_24H_MS


@dataclasses.dataclass
class VenueLeg:
    """One (venue-kind, descriptor) funding leg for a coin."""

    kind: str  # hl | dbt | bn | okx | bybit
    descriptor: str


def cvfc_legs(coin: str) -> list[VenueLeg]:
    """The four CVFC legs for a coin, in the doc's venue order."""
    return [
        VenueLeg("hl", f"hyperliquid:{coin}"),
        VenueLeg("dbt", f"deribit:{coin}_USDC-PERPETUAL"),
        VenueLeg("bn", f"binance-usdm:{coin.lower()}usdt"),
        VenueLeg("okx", f"okx:{coin}-USDT-SWAP"),
    ]


def read_rates(
    conn: sqlite3.Connection, descriptor: str, since_ms: int
) -> list[tuple[int, float]]:
    """Funding prints for one descriptor since ``since_ms``, ascending."""
    cur = conn.execute(
        "SELECT ts_ms, rate FROM funding WHERE descriptor = ? AND ts_ms >= ?"
        " ORDER BY ts_ms ASC",
        (descriptor, since_ms),
    )
    return [(int(r[0]), float(r[1])) for r in cur.fetchall()]


def apr_from_prints(
    rows: list[tuple[int, float]], window_ms: int, now_ms: int, descriptor: str
) -> float | None:
    """Annualized funding over the trailing window per the cadence law.

    ``None`` when the window holds no prints (data absence is honest —
    callers skip the leg, never assume 0)."""
    lo = now_ms - window_ms
    total = 0.0
    n = 0
    for ts, rate in rows:
        if ts >= lo and ts <= now_ms:
            total += rate
            n += 1
    if n == 0:
        return None
    if descriptor.startswith("deribit:"):
        # Hourly samples of an 8h rolling interest — see module docs.
        total /= 8.0
    days = window_ms / 86_400_000.0
    return (total / days) * 365.0


@dataclasses.dataclass
class CoinBoard:
    """Per-coin APR board: kind → (apr_24h, prints_24h)."""

    coin: str
    aprs: dict[str, float]

    def best_pair(self) -> tuple[str, str, float] | None:
        """Argmax ordered (short_kind, long_kind, spread) over legs
        with data; None when < 2 legs have data."""
        kinds = sorted(self.aprs)
        best: tuple[str, str, float] | None = None
        for s in kinds:
            for lo in kinds:
                if s == lo:
                    continue
                spread = self.aprs[s] - self.aprs[lo]
                if best is None or spread > best[2]:
                    best = (s, lo, spread)
        return best

    def pair_spread(self, short_kind: str, long_kind: str) -> float | None:
        if short_kind not in self.aprs or long_kind not in self.aprs:
            return None
        return self.aprs[short_kind] - self.aprs[long_kind]


def build_board(
    conn: sqlite3.Connection, coin: str, now_ms: int
) -> CoinBoard:
    aprs: dict[str, float] = {}
    for leg in cvfc_legs(coin):
        rows = read_rates(conn, leg.descriptor, now_ms - WINDOW_24H_MS)
        apr = apr_from_prints(rows, WINDOW_24H_MS, now_ms, leg.descriptor)
        if apr is not None:
            aprs[leg.kind] = apr
    return CoinBoard(coin, aprs)


def leg_descriptor(coin: str, kind: str) -> str:
    for leg in cvfc_legs(coin):
        if leg.kind == kind:
            return leg.descriptor
    raise KeyError(kind)


# ---------------------------------------------------------------
# Features (marks for crossing prices) + market map (sym resolution)
# ---------------------------------------------------------------


def load_marks(features_dir: pathlib.Path) -> dict[int, tuple[float, float]]:
    """sym → (last_bid, last_ask) in FLOAT dollars, from the newest
    fetch run's feature files (px fields are 1e6-scaled ints)."""
    runs = sorted(
        (p for p in features_dir.iterdir() if p.name.startswith("run-")),
        key=lambda p: p.name,
    )
    out: dict[int, tuple[float, float]] = {}
    if not runs:
        return out
    for f in runs[-1].glob("*.json"):
        try:
            d = json.loads(f.read_text())
        except (OSError, ValueError):
            continue
        sym = d.get("sym")
        bid = d.get("last_bid_px")
        ask = d.get("last_ask_px")
        if isinstance(sym, int) and isinstance(bid, int) and isinstance(ask, int):
            if bid > 0 and ask > 0:
                out[sym] = (bid / 1e6, ask / 1e6)
    return out


def load_map(map_path: pathlib.Path) -> dict[str, int]:
    try:
        d = json.loads(map_path.read_text())
    except (OSError, ValueError):
        return {}
    markets = d.get("markets")
    return dict(markets) if isinstance(markets, dict) else {}


# ---------------------------------------------------------------
# Positions state
# ---------------------------------------------------------------


def load_state(path: pathlib.Path) -> dict:
    try:
        d = json.loads(path.read_text())
    except (OSError, ValueError):
        return {"positions": []}
    if not isinstance(d, dict) or not isinstance(d.get("positions"), list):
        return {"positions": []}
    return d


def save_state(path: pathlib.Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(state, indent=1, sort_keys=True) + "\n")


# ---------------------------------------------------------------
# Decision + intent rendering
# ---------------------------------------------------------------


@dataclasses.dataclass
class Intent:
    tag: str
    descriptor: str
    sym: int
    venue: str
    side: str  # bid | ask
    px: float
    qty: float
    ttl_s: float


def crossing_px(side: str, bid: float, ask: float) -> float:
    if side == "bid":
        return round(ask * (1.0 + CROSS_SLIP), 9)
    return round(bid * (1.0 - CROSS_SLIP), 9)


def leg_intents(
    tag: str,
    coin: str,
    short_kind: str,
    long_kind: str,
    close: bool,
    market_map: dict[str, int],
    marks: dict[int, tuple[float, float]],
) -> tuple[list[Intent], list[str]]:
    """Two crossing legs for a pair (or its close). Returns
    (intents, skip_reasons) — any unaddressable/unpriced leg skips the
    WHOLE pair (a one-legged carry is not the strategy)."""
    intents: list[Intent] = []
    skips: list[str] = []
    for kind, opens_side in ((short_kind, "ask"), (long_kind, "bid")):
        side = opens_side if not close else ("bid" if opens_side == "ask" else "ask")
        desc = leg_descriptor(coin, kind)
        prefix = desc.split(":", 1)[0]
        venue = INTENT_VENUE_BY_PREFIX.get(prefix)
        if venue is None:
            skips.append(f"{desc}: venue not intent-addressable")
            continue
        sym = market_map.get(desc)
        if sym is None:
            skips.append(f"{desc}: no market-map entry")
            continue
        mark = marks.get(sym)
        if mark is None:
            skips.append(f"{desc}: no captured mark (features)")
            continue
        px = crossing_px(side, mark[0], mark[1])
        if px <= 0:
            skips.append(f"{desc}: degenerate mark")
            continue
        qty = round(LEG_NOTIONAL_USD / px, 6)
        intents.append(Intent(tag, desc, sym, venue, side, px, qty, INTENT_TTL_S))
    if skips:
        return [], skips
    return intents, []


def decide_cvfc(
    conn: sqlite3.Connection,
    state: dict,
    market_map: dict[str, int],
    marks: dict[int, tuple[float, float]],
    now_ms: int,
) -> tuple[list[Intent], list[str], list[str]]:
    """CVFC entries/exits per the doc spec. Returns
    (intents, digest_lines, skip_notes); mutates ``state``."""
    intents: list[Intent] = []
    lines: list[str] = []
    notes: list[str] = []
    exited_this_cycle: set[str] = set()
    held: dict[str, dict] = {p["coin"]: p for p in state["positions"]}

    boards = {c: build_board(conn, c, now_ms) for c in CVFC_COINS}
    for coin in CVFC_COINS:
        b = boards[coin]
        pretty = " ".join(f"{k}={v:+.1%}" for k, v in sorted(b.aprs.items()))
        lines.append(f"  {coin:5s} {pretty if pretty else '(no funding data)'}")

    # Exits first (free slots per the spec's hysteresis).
    for coin, pos in list(held.items()):
        b = boards.get(coin)
        spread = (
            b.pair_spread(pos["short"], pos["long"]) if b is not None else None
        )
        age_h = (now_ms - pos["entered_ms"]) / 3_600_000.0
        if spread is None:
            lines.append(f"  HELD {coin} {pos['short']}/{pos['long']}: no data, hold")
            continue
        if spread < 0.0 and age_h >= EXIT_MIN_HOLD_H:
            legs, skips = leg_intents(
                f"cvfc-exit-{coin}", coin, pos["short"], pos["long"], True,
                market_map, marks,
            )
            if skips:
                notes.extend(skips)
                lines.append(f"  EXIT-BLOCKED {coin}: {'; '.join(skips)}")
                continue
            intents.extend(legs)
            state["positions"] = [
                p for p in state["positions"] if p["coin"] != coin
            ]
            del held[coin]
            exited_this_cycle.add(coin)
            lines.append(
                f"  EXIT {coin} {pos['short']}/{pos['long']}"
                f" spread={spread:+.1%} age={age_h:.0f}h"
            )
        else:
            lines.append(
                f"  HELD {coin} {pos['short']}/{pos['long']}"
                f" spread={spread:+.1%} age={age_h:.0f}h (exit<0 after {EXIT_MIN_HOLD_H:.0f}h)"
            )

    # Entries. A coin exited THIS cycle sits out one cycle — the
    # reversed pair may be a legitimate new entry (the doc's ADA
    # short-DBT example), but exit+re-enter in one push is churn.
    for coin in CVFC_COINS:
        if coin in held or coin in CVFC_MAJORS_EXCLUDED:
            continue
        if coin in exited_this_cycle:
            lines.append(f"  COOLDOWN {coin}: exited this cycle, re-eligible next")
            continue
        if len(state["positions"]) >= MAX_POSITIONS:
            break
        b = boards[coin]
        best = b.best_pair()
        if best is None:
            continue
        short_kind, long_kind, spread = best
        if spread < ENTRY_SPREAD_APR:
            continue
        legs, skips = leg_intents(
            f"cvfc-entry-{coin}", coin, short_kind, long_kind, False,
            market_map, marks,
        )
        if skips:
            notes.extend(skips)
            lines.append(
                f"  ENTRY-SIGNAL {coin} short={short_kind} long={long_kind}"
                f" spread={spread:+.1%} — NOT executable: {'; '.join(skips)}"
            )
            continue
        intents.extend(legs)
        state["positions"].append(
            {
                "coin": coin,
                "short": short_kind,
                "long": long_kind,
                "entered_ms": now_ms,
                "entry_spread": spread,
            }
        )
        lines.append(
            f"  ENTER {coin} short={short_kind} long={long_kind} spread={spread:+.1%}"
        )
    return intents, lines, notes


def explicit_leg_intents(
    tag: str,
    leg_specs: list[tuple[str, str]],
    market_map: dict[str, int],
    marks: dict[int, tuple[float, float]],
) -> tuple[list[Intent], list[str]]:
    """Crossing intents for explicit (descriptor, side) legs — all
    legs must be addressable+priced or the pair is skipped whole."""
    intents: list[Intent] = []
    skips: list[str] = []
    for desc, side in leg_specs:
        prefix = desc.split(":", 1)[0]
        venue = INTENT_VENUE_BY_PREFIX.get(prefix)
        if venue is None:
            skips.append(f"{desc}: venue not intent-addressable")
            continue
        sym = market_map.get(desc)
        if sym is None:
            skips.append(f"{desc}: no market-map entry")
            continue
        mark = marks.get(sym)
        if mark is None:
            skips.append(f"{desc}: no captured mark (features)")
            continue
        px = crossing_px(side, mark[0], mark[1])
        if px <= 0:
            skips.append(f"{desc}: degenerate mark")
            continue
        qty = round(LEG_NOTIONAL_USD / px, 6)
        intents.append(Intent(tag, desc, sym, venue, side, px, qty, INTENT_TTL_S))
    if skips:
        return [], skips
    return intents, []


def decide_s1(
    conn: sqlite3.Connection,
    state: dict,
    market_map: dict[str, int],
    marks: dict[int, tuple[float, float]],
    now_ms: int,
) -> tuple[list[Intent], list[str], list[str]]:
    """S1 pilot per Consolidated §5.1 — executable pairs since the
    bybit unfreeze. Mutates ``state['s1_positions']``."""
    intents: list[Intent] = []
    notes: list[str] = []
    positions: list[dict] = state.setdefault("s1_positions", [])
    held = {p["name"]: p for p in positions}
    lines: list[str] = []
    for name in S1_PILOT:
        bn_desc = f"binance-usdm:{name.lower()}"
        by_desc = f"bybit-linear:{name}"
        bn24 = apr_from_prints(
            read_rates(conn, bn_desc, now_ms - WINDOW_24H_MS),
            WINDOW_24H_MS, now_ms, bn_desc,
        )
        by24 = apr_from_prints(
            read_rates(conn, by_desc, now_ms - WINDOW_24H_MS),
            WINDOW_24H_MS, now_ms, by_desc,
        )
        bn3 = apr_from_prints(
            read_rates(conn, bn_desc, now_ms - WINDOW_3D_MS),
            WINDOW_3D_MS, now_ms, bn_desc,
        )
        by3 = apr_from_prints(
            read_rates(conn, by_desc, now_ms - WINDOW_3D_MS),
            WINDOW_3D_MS, now_ms, by_desc,
        )
        if bn24 is None or by24 is None:
            lines.append(f"  {name:13s} (missing funding data)")
            continue
        sp24 = bn24 - by24
        sp3 = (bn3 - by3) if (bn3 is not None and by3 is not None) else None
        qualifies = abs(sp24) >= S1_ENTRY_ABS_APR and (
            sp3 is not None and abs(sp3) >= S1_CONFIRM_3D_APR
        )
        s3txt = f"{sp3:+.1%}" if sp3 is not None else "n/a"

        pos = held.get(name)
        if pos is not None:
            # Directional held spread: apr(short venue) − apr(long).
            aprs = {"bn": bn24, "bybit": by24}
            directional = aprs[pos["short"]] - aprs[pos["long"]]
            age_h = (now_ms - pos["entered_ms"]) / 3_600_000.0
            if directional < S1_EXIT_DIRECTIONAL_APR or age_h > S1_MAX_AGE_H:
                legs, skips = explicit_leg_intents(
                    f"s1-exit-{name}",
                    [
                        (pos["short_desc"], "bid"),
                        (pos["long_desc"], "ask"),
                    ],
                    market_map,
                    marks,
                )
                if skips:
                    notes.extend(skips)
                    lines.append(f"  {name:13s} EXIT-BLOCKED: {'; '.join(skips)}")
                    continue
                intents.extend(legs)
                positions[:] = [p for p in positions if p["name"] != name]
                lines.append(
                    f"  {name:13s} EXIT directional={directional:+.1%} age={age_h:.0f}h"
                )
            else:
                lines.append(
                    f"  {name:13s} HELD directional={directional:+.1%} age={age_h:.0f}h"
                )
            continue

        if qualifies and len(positions) < S1_MAX_POSITIONS:
            # Long the more-negative venue, short the other (§5.1).
            if bn24 < by24:
                long_kind, long_desc = "bn", f"binance-usdm:{name.lower()}"
                short_kind, short_desc = "bybit", f"bybit-linear:{name}"
            else:
                long_kind, long_desc = "bybit", f"bybit-linear:{name}"
                short_kind, short_desc = "bn", f"binance-usdm:{name.lower()}"
            legs, skips = explicit_leg_intents(
                f"s1-entry-{name}",
                [(short_desc, "ask"), (long_desc, "bid")],
                market_map,
                marks,
            )
            if skips:
                notes.extend(skips)
                lines.append(
                    f"  {name:13s} spread24={sp24:+.1%} spread3d={s3txt}"
                    f" QUALIFIES — NOT executable: {'; '.join(skips)}"
                )
                continue
            intents.extend(legs)
            positions.append(
                {
                    "name": name,
                    "short": short_kind,
                    "long": long_kind,
                    "short_desc": short_desc,
                    "long_desc": long_desc,
                    "entered_ms": now_ms,
                    "entry_spread": sp24,
                }
            )
            lines.append(
                f"  {name:13s} ENTER short={short_kind} long={long_kind}"
                f" spread24={sp24:+.1%} spread3d={s3txt}"
            )
        else:
            lines.append(
                f"  {name:13s} spread24={sp24:+.1%} spread3d={s3txt}"
                f" {'QUALIFIES (slots full)' if qualifies else ''}"
            )
    return intents, lines, notes


# ---------------------------------------------------------------
# Output rendering
# ---------------------------------------------------------------


def render_push_sh(intents: list[Intent]) -> str:
    out = ["#!/bin/sh", "# generated by claude_worker.carry_signal — review, then run"]
    out.append("set -e")
    for i in intents:
        out.append(
            "uv run claude-worker push --kind order-intent"
            f" --sym {i.sym} --venue {i.venue} --side {i.side}"
            f" --px {i.px} --qty {i.qty} --ttl-s {i.ttl_s}"
        )
    return "\n".join(out) + "\n"


def batch_dict(intents: list[Intent], now_ms: int) -> dict:
    return {
        "schema": 1,
        "generated_ms": now_ms,
        "intents": [dataclasses.asdict(i) for i in intents],
    }


def run_cycle(
    db_path: pathlib.Path,
    features_dir: pathlib.Path,
    map_path: pathlib.Path,
    out_dir: pathlib.Path,
    now_ms: int | None = None,
) -> pathlib.Path:
    """One full cycle; returns the digest path. Pure files+SQLite."""
    now = int(time.time() * 1000) if now_ms is None else now_ms
    conn = sqlite3.connect(str(db_path))
    try:
        market_map = load_map(map_path)
        marks = load_marks(features_dir)
        state_path = out_dir / "state.json"
        state = load_state(state_path)
        intents, cvfc_lines, notes = decide_cvfc(
            conn, state, market_map, marks, now
        )
        s1_intents, s1_lines, s1_notes = decide_s1(
            conn, state, market_map, marks, now
        )
        intents.extend(s1_intents)
        notes.extend(s1_notes)
    finally:
        conn.close()

    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.datetime.fromtimestamp(
        now / 1000, tz=datetime.timezone.utc
    ).strftime("%Y%m%dT%H%M%SZ")
    (out_dir / f"batch-{stamp}.json").write_text(
        json.dumps(batch_dict(intents, now), indent=1, sort_keys=True) + "\n"
    )
    (out_dir / "push.sh").write_text(render_push_sh(intents))
    digest = out_dir / f"digest-{stamp}.txt"
    body = [
        f"carry_signal digest {stamp} (funding source: candles.db;"
        f" cadence law incl. deribit interest_8h/8)",
        "CVFC-1 board (24h APR by leg; majors excluded from entry):",
        *cvfc_lines,
        "S1 pilot (BN vs Bybit; executable since the bybit unfreeze):",
        *s1_lines,
        f"intents: {len(intents)}"
        + (f" — push.sh ready" if intents else " — nothing to push"),
    ]
    if notes:
        body.append("skips: " + "; ".join(sorted(set(notes))))
    digest.write_text("\n".join(body) + "\n")
    save_state(state_path, state)
    return digest


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="claude_worker.carry_signal")
    ap.add_argument("--db", default=os.environ.get(
        "CLAUDE_WORKER_CANDLES_DB", "~/multivenue/worker/candles.db"))
    ap.add_argument("--features-dir", default=os.environ.get(
        "CLAUDE_WORKER_FEATURES_DIR", "~/multivenue/worker/features"))
    ap.add_argument("--market-map", default=os.environ.get(
        "CLAUDE_WORKER_MARKET_MAP", "~/multivenue/worker/market-map.json"))
    ap.add_argument("--out", default="~/multivenue/worker/carry")
    args = ap.parse_args(argv)
    digest = run_cycle(
        pathlib.Path(os.path.expanduser(args.db)),
        pathlib.Path(os.path.expanduser(args.features_dir)),
        pathlib.Path(os.path.expanduser(args.market_map)),
        pathlib.Path(os.path.expanduser(args.out)),
    )
    sys.stdout.write(digest.read_text())
    sys.stdout.write(f"[digest] {digest}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
