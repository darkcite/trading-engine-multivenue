# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""carry_signal tests — pure files+SQLite, no network by construction."""

import json
import pathlib
import sqlite3

import claude_worker.carry_signal
import claude_worker.funding

HOUR_MS = 3_600_000
NOW = 1_788_000_000_000


def make_db(tmp_path: pathlib.Path) -> pathlib.Path:
    p = tmp_path / "candles.db"
    conn = sqlite3.connect(str(p))
    claude_worker.funding.ensure_schema(conn)
    conn.commit()
    conn.close()
    return p


def put_rates(db: pathlib.Path, descriptor: str, rows: list[tuple[int, float]]):
    conn = sqlite3.connect(str(db))
    conn.executemany(
        "INSERT OR IGNORE INTO funding (venue, descriptor, ts_ms, rate, fetched_ts)"
        " VALUES (0, ?, ?, ?, 0)",
        [(descriptor, ts, r) for ts, r in rows],
    )
    conn.commit()
    conn.close()


def hourly(rate: float, hours: int = 24, end: int = NOW) -> list[tuple[int, float]]:
    return [(end - i * HOUR_MS, rate) for i in range(1, hours + 1)]


def eight_hourly(rate: float, prints: int = 3, end: int = NOW) -> list[tuple[int, float]]:
    return [(end - i * 8 * HOUR_MS, rate) for i in range(1, prints + 1)]


# ---------------------------------------------------------------
# Cadence law (R4 lesson 9)
# ---------------------------------------------------------------


def test_apr_hourly_prints_sum_to_daily_times_365(tmp_path):
    db = make_db(tmp_path)
    # 24 hourly prints of 1e-5 => daily 2.4e-4 => APR 8.76%.
    put_rates(db, "hyperliquid:ADA", hourly(1e-5))
    conn = sqlite3.connect(str(db))
    rows = claude_worker.carry_signal.read_rates(
        conn, "hyperliquid:ADA", NOW - claude_worker.carry_signal.WINDOW_24H_MS
    )
    apr = claude_worker.carry_signal.apr_from_prints(
        rows, claude_worker.carry_signal.WINDOW_24H_MS, NOW, "hyperliquid:ADA"
    )
    conn.close()
    assert apr is not None
    assert abs(apr - 1e-5 * 24 * 365) < 1e-9


def test_apr_deribit_hourly_interest8h_divides_by_8(tmp_path):
    db = make_db(tmp_path)
    # Hourly SAMPLES of an 8h interest: naive sum over-counts 8x.
    put_rates(db, "deribit:ADA_USDC-PERPETUAL", hourly(8e-5))
    conn = sqlite3.connect(str(db))
    rows = claude_worker.carry_signal.read_rates(
        conn,
        "deribit:ADA_USDC-PERPETUAL",
        NOW - claude_worker.carry_signal.WINDOW_24H_MS,
    )
    apr = claude_worker.carry_signal.apr_from_prints(
        rows,
        claude_worker.carry_signal.WINDOW_24H_MS,
        NOW,
        "deribit:ADA_USDC-PERPETUAL",
    )
    conn.close()
    assert apr is not None
    # (24 * 8e-5 / 8) * 365 == 24 * 1e-5 * 365
    assert abs(apr - 1e-5 * 24 * 365) < 1e-9


def test_apr_absent_window_is_none_not_zero(tmp_path):
    db = make_db(tmp_path)
    conn = sqlite3.connect(str(db))
    apr = claude_worker.carry_signal.apr_from_prints(
        [], claude_worker.carry_signal.WINDOW_24H_MS, NOW, "binance-usdm:adausdt"
    )
    conn.close()
    assert apr is None


# ---------------------------------------------------------------
# Fixture world: ADA rich on HL, deeply negative on Deribit
# ---------------------------------------------------------------


def fixture_world(tmp_path, ada_dbt_rate=-4e-5):
    db = make_db(tmp_path)
    # HL hourly +2e-5/h  => +17.5% APR
    put_rates(db, "hyperliquid:ADA", hourly(2e-5))
    # Deribit hourly interest_8h of ada_dbt_rate => APR = r*3*365
    put_rates(db, "deribit:ADA_USDC-PERPETUAL", hourly(ada_dbt_rate))
    # BN 8h prints ~ +1e-4/8h => +10.95% (mid venue)
    put_rates(db, "binance-usdm:adausdt", eight_hourly(1e-4))
    # majors present but excluded from entry
    put_rates(db, "hyperliquid:BTC", hourly(2e-5))
    put_rates(db, "deribit:BTC_USDC-PERPETUAL", hourly(-4e-5))

    features = tmp_path / "features" / "run-1"
    features.mkdir(parents=True)
    marks = {
        7001: (0.199, 0.2),  # hl ADA
        7002: (0.199, 0.2),  # dbt ADA perp
    }
    for sym, (bid, ask) in marks.items():
        (features / f"{sym}.json").write_text(
            json.dumps(
                {
                    "sym": sym,
                    "last_bid_px": int(bid * 1e6),
                    "last_ask_px": int(ask * 1e6),
                }
            )
        )
    map_path = tmp_path / "market-map.json"
    map_path.write_text(
        json.dumps(
            {
                "markets": {
                    "hyperliquid:ADA": 7001,
                    "deribit:ADA_USDC-PERPETUAL": 7002,
                }
            }
        )
    )
    out = tmp_path / "carry"
    return db, tmp_path / "features", map_path, out


def run(tmp_path, db, features, map_path, out, now=NOW):
    return claude_worker.carry_signal.run_cycle(db, features, map_path, out, now)


def test_cvfc_enters_hl_short_dbt_long_with_crossing_caps(tmp_path):
    db, features, map_path, out = fixture_world(tmp_path)
    digest = run(tmp_path, db, features, map_path, out)
    body = digest.read_text()
    assert "ENTER ADA short=hl long=dbt" in body
    batch = json.loads(sorted(out.glob("batch-*.json"))[0].read_text())
    assert len(batch["intents"]) == 2
    by_side = {i["side"]: i for i in batch["intents"]}
    # short leg = ask on HL, crossing DOWN through the bid
    assert by_side["ask"]["venue"] == "hyperliquid"
    assert by_side["ask"]["px"] < 0.199
    # long leg = bid on Deribit, crossing UP through the ask
    assert by_side["bid"]["venue"] == "deribit"
    assert by_side["bid"]["px"] > 0.2
    for i in batch["intents"]:
        assert i["px"] * i["qty"] <= 100.0  # validator order cap
    sh = (out / "push.sh").read_text()
    assert sh.count("--kind order-intent") == 2
    assert "--venue hyperliquid" in sh and "--venue deribit" in sh


def test_cvfc_majors_never_enter(tmp_path):
    db, features, map_path, out = fixture_world(tmp_path)
    digest = run(tmp_path, db, features, map_path, out)
    assert "ENTER BTC" not in digest.read_text()


def test_cvfc_exit_needs_min_hold_and_negative_spread(tmp_path):
    db, features, map_path, out = fixture_world(tmp_path)
    run(tmp_path, db, features, map_path, out)
    state = json.loads((out / "state.json").read_text())
    assert len(state["positions"]) == 1

    # Spread flips negative 10h later: held (min-hold binds).
    later = NOW + 10 * HOUR_MS
    put_rates(db, "hyperliquid:ADA", hourly(-6e-5, end=later))
    put_rates(db, "deribit:ADA_USDC-PERPETUAL", hourly(1e-5, end=later))
    digest2 = run(tmp_path, db, features, map_path, out, now=later)
    assert "HELD ADA" in digest2.read_text()

    # Past 96h with the flip still on: exits with two closing legs.
    late = NOW + 100 * HOUR_MS
    put_rates(db, "hyperliquid:ADA", hourly(-6e-5, end=late))
    put_rates(db, "deribit:ADA_USDC-PERPETUAL", hourly(1e-5, end=late))
    digest3 = run(tmp_path, db, features, map_path, out, now=late)
    assert "EXIT ADA" in digest3.read_text()
    state3 = json.loads((out / "state.json").read_text())
    assert state3["positions"] == []
    batches = sorted(out.glob("batch-*.json"))
    close_batch = json.loads(batches[-1].read_text())
    sides = sorted(i["side"] for i in close_batch["intents"])
    assert sides == ["ask", "bid"]  # closing both legs


def test_cvfc_signal_without_marks_is_reported_not_pushed(tmp_path):
    db, features, map_path, out = fixture_world(tmp_path)
    # Kill the marks: signal must surface as NOT executable.
    for f in (features / "run-1").glob("*.json"):
        f.unlink()
    digest = run(tmp_path, db, features, map_path, out)
    body = digest.read_text()
    assert "ENTRY-SIGNAL ADA" in body and "NOT executable" in body
    batch = json.loads(sorted(out.glob("batch-*.json"))[0].read_text())
    assert batch["intents"] == []
    state = json.loads((out / "state.json").read_text())
    assert state["positions"] == []  # nothing recorded as held


def test_s1_pilot_is_digest_only(tmp_path):
    db, features, map_path, out = fixture_world(tmp_path)
    # A qualifying S1 name: |spread24| >= 50% ann with 3d confirm.
    put_rates(db, "binance-usdm:cotiusdt", eight_hourly(-2e-3, prints=9))
    put_rates(db, "bybit-linear:COTIUSDT", eight_hourly(1e-4, prints=9))
    digest = run(tmp_path, db, features, map_path, out)
    body = digest.read_text()
    assert "COTIUSDT" in body and "QUALIFIES" in body
    batch = json.loads(sorted(out.glob("batch-*.json"))[0].read_text())
    for i in batch["intents"]:
        assert "bybit" not in i["venue"]


def test_push_sh_renders_exact_verb_lines(tmp_path):
    db, features, map_path, out = fixture_world(tmp_path)
    run(tmp_path, db, features, map_path, out)
    sh = (out / "push.sh").read_text()
    for line in sh.splitlines():
        if line.startswith("uv run"):
            assert line.startswith(
                "uv run claude-worker push --kind order-intent --sym "
            )
            assert "--ttl-s 3600.0" in line
