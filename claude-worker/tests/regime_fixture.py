# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Shared regime-lane fixtures (RG5): a ``regime.toml`` cut from the
example with test members, and a ``candles.db`` holding a steady
uptrend for BTC + the members plus funding prints — enough for the
worker evaluator to judge a word. Used by ``test_regime_lane`` and the
serve-cycle tests. Convention: full ``import x`` only."""

import pathlib

import claude_worker.candles

EXAMPLE = pathlib.Path(__file__).resolve().parents[2] / "regime.toml.example"
MEMBERS = ("binance-usdm:ethusdt", "binance-usdm:solusdt", "binance-usdm:bnbusdt", "binance-usdm:xrpusdt")
BTC = "binance-usdm:btcusdt"
NOW_MS = 1_800_000_000_000  # a minute boundary (÷ 60_000)


def artifact(tmp_path: pathlib.Path, members: tuple[str, ...] = MEMBERS) -> pathlib.Path:
    text = EXAMPLE.read_text(encoding="utf-8")
    quoted = ", ".join(f'"{m}"' for m in members)
    out: list[str] = []
    for line in text.splitlines(keepends=True):
        out.append(f"members = [{quoted}]\n" if line.startswith("members = ") else line)
    path = tmp_path / "regime.toml"
    path.write_text("".join(out), encoding="utf-8")
    return path


def candles_db(
    tmp_path: pathlib.Path, minutes: int = 400, lag_min: int = 0, bps_per_min: float = 20.0
) -> pathlib.Path:
    """1 m closes for BTC + members rising ``bps_per_min`` per minute up to
    ``lag_min`` minutes before NOW's minute, plus 12 funding prints."""
    db = tmp_path / "candles.db"
    conn = claude_worker.candles.open_db(db)
    last_minute = NOW_MS // 60_000 - 1 - lag_min
    rows = []
    for k in range(minutes):
        minute = last_minute - (minutes - 1 - k)
        px = 100_000.0 * (1 + bps_per_min / 10_000) ** k
        for desc in (BTC, *MEMBERS):
            rows.append((1, desc, "1m", minute * 60_000, px, px, px, px, 1.0, "rest", NOW_MS))
    conn.executemany(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        rows,
    )
    conn.executescript(
        "CREATE TABLE IF NOT EXISTS funding (venue INTEGER NOT NULL, descriptor TEXT NOT NULL,"
        " ts_ms INTEGER NOT NULL, rate REAL NOT NULL, fetched_ts INTEGER NOT NULL,"
        " PRIMARY KEY (venue, descriptor, ts_ms)) WITHOUT ROWID;"
    )
    conn.executemany(
        "INSERT INTO funding (venue,descriptor,ts_ms,rate,fetched_ts) VALUES (?,?,?,?,?)",
        [(1, BTC, NOW_MS - (12 - i) * 8 * 3_600_000, 0.0001 * (i - 4), NOW_MS) for i in range(12)],
    )
    conn.commit()
    conn.close()
    return db
