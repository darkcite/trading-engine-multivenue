# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Tests for the bar-level backtest runner.

The load-bearing ones are `test_no_lookahead_entry_is_next_bar_open` (a
backtest that peeks is worthless) and
`test_report_round_trips_through_pnl_report` (the whole point of the
frozen contract is that the EXISTING P&L lane reads it).

Convention: full ``import x`` only.
"""

import math
import pathlib
import sqlite3
import tempfile

import pytest

import claude_worker.bartest
import claude_worker.pnl_report

MS_1H = 3_600_000
T0 = 1_773_000_000_000 // MS_1H * MS_1H  # an hour-aligned epoch


def make_bars(descriptor, opens, tf_ms=MS_1H, start=T0):
    """A synthetic series: `opens[i]` is bar i's open, close = next open."""
    stamps = tuple(start + i * tf_ms for i in range(len(opens)))
    return claude_worker.bartest.Bars(
        descriptor=descriptor,
        tf_ms=tf_ms,
        open_ts=stamps,
        open_px={t: float(p) for t, p in zip(stamps, opens)},
        close_px={t: float(p) for t, p in zip(stamps, opens)},
    )


TIERS = {
    "hl": claude_worker.bartest.FeeTier(2, 5),
    "bn": claude_worker.bartest.FeeTier(10, 10),
}


# ---------------------------------------------------------------- fees


def test_fee_tiers_parse_the_d2_amend_shape():
    with tempfile.TemporaryDirectory() as d:
        p = pathlib.Path(d) / "fees.toml"
        p.write_text(
            '# header comment\n[fees]\npm = "0:350"\nbn = "10:10"\n'
            'hl = "2:5"   # trailing comment\n',
            encoding="utf-8",
        )
        tiers = claude_worker.bartest.read_fee_tiers(p)
    assert tiers["pm"] == claude_worker.bartest.FeeTier(0, 350)
    assert tiers["bn"] == claude_worker.bartest.FeeTier(10, 10)
    assert tiers["hl"] == claude_worker.bartest.FeeTier(2, 5)


def test_missing_fees_file_is_fail_fast():
    with pytest.raises(claude_worker.bartest.BartestError):
        claude_worker.bartest.read_fee_tiers("/nonexistent/fees.toml")


def test_venue_slot_mapping_and_unknown_descriptor():
    assert claude_worker.bartest.venue_of("hyperliquid:BTC") == "hl"
    assert claude_worker.bartest.venue_of("binance-usdm:btcusdt") == "bn"
    assert claude_worker.bartest.venue_of("bybit-linear:BTCUSDT") == "bybit"
    with pytest.raises(claude_worker.bartest.BartestError):
        claude_worker.bartest.venue_of("kraken:BTCUSD")


def test_round_trip_fee_is_two_sides_at_the_venue_tier():
    """hl taker is 5 bps; $10,000 of notional round-tripped costs $10."""
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 10)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=4, notional_usd=10_000.0, tiers=TIERS, slippage_bps=0.0
    )
    assert res.n_trades == 1
    assert res.fees_usd == pytest.approx(10.0)
    assert res.gross_usd == pytest.approx(0.0)
    assert res.net_usd == pytest.approx(-10.0)


def test_maker_variant_uses_the_maker_slot():
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 10)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=4, notional_usd=10_000.0, tiers=TIERS,
        slippage_bps=0.0, maker=True,
    )
    assert res.fees_usd == pytest.approx(4.0)  # 2 bps x 2 sides


def test_execution_modes_charge_the_right_leg_rates():
    """taker = 2x taker; maker = 2x maker; maker_entry = one of each."""
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 10)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    got = {}
    for mode in claude_worker.bartest.EXECUTION_MODES:
        res = claude_worker.bartest.run(
            bars, reb, hold_bars=4, notional_usd=10_000.0, tiers=TIERS,
            slippage_bps=0.0, execution=mode,
        )
        got[mode] = res.fees_usd
    assert got["taker"] == pytest.approx(10.0)        # 5 + 5 bps
    assert got["maker"] == pytest.approx(4.0)         # 2 + 2 bps
    assert got["maker_entry"] == pytest.approx(7.0)   # 2 + 5 bps


def test_resting_leg_pays_no_slippage():
    """A quote that rests does not cross the spread it is inside."""
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 10)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    slips = {}
    for mode in claude_worker.bartest.EXECUTION_MODES:
        res = claude_worker.bartest.run(
            bars, reb, hold_bars=4, notional_usd=10_000.0, tiers=TIERS,
            slippage_bps=3.0, execution=mode,
        )
        slips[mode] = res.slip_usd
    assert slips["taker"] == pytest.approx(6.0)        # both legs cross
    assert slips["maker_entry"] == pytest.approx(3.0)  # exit only
    assert slips["maker"] == pytest.approx(0.0)        # neither


def test_legacy_maker_flag_still_selects_maker():
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 10)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=4, notional_usd=10_000.0, tiers=TIERS,
        slippage_bps=0.0, maker=True,
    )
    assert res.execution == "maker"
    assert res.fees_usd == pytest.approx(4.0)


def test_unknown_execution_mode_is_fail_fast():
    with pytest.raises(claude_worker.bartest.BartestError):
        claude_worker.bartest.run(
            {}, [], hold_bars=1, notional_usd=1.0, tiers=TIERS,
            execution="iceberg",
        )


def test_newey_west_inflates_the_se_on_overlapping_periods():
    """Overlapping holds share holding time, so the naive SE understates.
    The correction must widen it, never narrow it, on correlated data."""
    # A slow swing plus drift: overlapping 8 h returns then genuinely
    # co-move AND have real variance. (A constant-growth series has zero
    # return variance, so both SEs collapse to float noise and the test
    # measures nothing -- that is what the first version of this fixture
    # did.)
    opens = [
        100.0 * math.exp(0.05 * math.sin(i / 6.0) + 0.002 * i)
        for i in range(60)
    ]
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", opens)}
    hold = 8
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0 + k * MS_1H, weights={"hyperliquid:BTC": 1.0}
        )
        for k in range(40)
    ]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=hold, notional_usd=1_000.0, tiers=TIERS,
        slippage_bps=0.0,
    )
    naive = res.stats(periods_per_year=1000.0, overlap=1)
    corrected = res.stats(periods_per_year=1000.0, overlap=hold)
    assert naive["overlap"] == 1 and corrected["overlap"] == hold
    assert corrected["se_bps"] > naive["se_bps"]
    assert corrected["nw_inflation"] > 1.0
    assert abs(corrected["t"]) < abs(naive["t"])


def test_overlap_one_leaves_the_se_untouched():
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0 + i for i in range(30)])}
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0 + k * 4 * MS_1H, weights={"hyperliquid:BTC": 1.0}
        )
        for k in range(6)
    ]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=4, notional_usd=1_000.0, tiers=TIERS, slippage_bps=0.0
    )
    s = res.stats(periods_per_year=1000.0, overlap=1)
    assert s["nw_inflation"] == pytest.approx(1.0)
    assert s["se_bps"] == pytest.approx(s["se_bps_naive"])


def test_slippage_is_charged_per_side_on_traded_notional():
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 10)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=4, notional_usd=10_000.0, tiers=TIERS, slippage_bps=3.0
    )
    assert res.slip_usd == pytest.approx(6.0)


# ------------------------------------------------------------ the fill law


def test_no_lookahead_entry_is_next_bar_open():
    """The decision at bar 0 must NOT execute at bar 0. Prices step by 1
    per bar, so the entry price identifies the bar unambiguously."""
    opens = [100.0, 101.0, 102.0, 103.0, 104.0, 105.0]
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", opens)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=2, notional_usd=1_000.0, tiers=TIERS, slippage_bps=0.0
    )
    trade = res.trades[0]
    assert trade.entry_px == 101.0  # bar 1's open, not bar 0's
    assert trade.exit_px == 103.0  # bar 1 + 2
    assert trade.entry_ts == T0 + MS_1H


def test_short_gains_when_price_falls():
    opens = [100.0, 100.0, 90.0, 90.0]
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", opens)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": -1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=1, notional_usd=1_000.0, tiers=TIERS, slippage_bps=0.0
    )
    assert res.gross_usd == pytest.approx(100.0)  # -1 * 1000 * (90/100 - 1)


def test_missing_exit_bar_is_skipped_not_priced_from_a_neighbour():
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0, 101.0, 102.0])}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=8, notional_usd=1_000.0, tiers=TIERS
    )
    assert res.n_trades == 0
    assert res.skipped["no_exit_bar"] == 1


def test_unknown_instrument_and_zero_weight_are_not_trades():
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 6)}
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0, weights={"hyperliquid:BTC": 0.0, "hyperliquid:ETH": 1.0}
        )
    ]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=2, notional_usd=1_000.0, tiers=TIERS
    )
    assert res.n_trades == 0
    assert res.skipped["no_series"] == 1


def test_hold_bars_must_be_positive():
    with pytest.raises(claude_worker.bartest.BartestError):
        claude_worker.bartest.run({}, [], hold_bars=0, notional_usd=1.0, tiers=TIERS)


def test_periods_are_non_overlapping_so_effective_n_is_the_period_count():
    """Stride == hold: five rebalances, two legs each, five independent
    observations -- not ten."""
    opens = [100.0 + i for i in range(24)]
    bars = {
        "hyperliquid:BTC": make_bars("hyperliquid:BTC", opens),
        "hyperliquid:ETH": make_bars("hyperliquid:ETH", opens),
    }
    hold = 4
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0 + k * hold * MS_1H,
            weights={"hyperliquid:BTC": 0.5, "hyperliquid:ETH": -0.5},
        )
        for k in range(5)
    ]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=hold, notional_usd=1_000.0, tiers=TIERS
    )
    assert res.n_trades == 10
    assert len(res.period_returns()) == 5


# ------------------------------------------------------- controls and nulls


def test_null_arm_preserves_the_weight_multiset_across_instruments():
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0, weights={"a:1": 1.0, "a:2": -1.0, "a:3": 0.5}
        )
    ]
    out = claude_worker.bartest.null_arm(reb, seed=7)
    assert sorted(out[0].weights.values()) == sorted(reb[0].weights.values())
    assert set(out[0].weights) == set(reb[0].weights)


def test_null_arm_across_time_permutes_whole_vectors():
    reb = [
        claude_worker.bartest.Rebalance(ts=T0 + i * MS_1H, weights={"a:1": float(i)})
        for i in range(6)
    ]
    out = claude_worker.bartest.null_arm(reb, seed=3, across="time")
    assert [r.ts for r in out] == [r.ts for r in reb]
    assert sorted(r.weights["a:1"] for r in out) == [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]


def test_unknown_null_axis_is_fail_fast():
    with pytest.raises(claude_worker.bartest.BartestError):
        claude_worker.bartest.null_arm([], across="sideways")


def test_always_long_and_one_side_controls():
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0, weights={"a:1": 1.0, "a:2": -1.0, "a:3": 0.0}
        )
    ]
    al = claude_worker.bartest.always_long(reb)
    assert al[0].weights == {"a:1": 1.0, "a:2": 1.0}
    assert claude_worker.bartest.one_side(reb, +1)[0].weights == {"a:1": 1.0}
    assert claude_worker.bartest.one_side(reb, -1)[0].weights == {"a:2": -1.0}


def test_split_halves_and_quarters():
    reb = [
        claude_worker.bartest.Rebalance(ts=T0 + i * 30 * 24 * MS_1H, weights={"a:1": 1.0})
        for i in range(6)
    ]
    first, second = claude_worker.bartest.split_halves(reb)
    assert len(first) == 3 and len(second) == 3
    assert first[-1].ts < second[0].ts
    quarters = claude_worker.bartest.by_quarter(reb)
    assert sum(len(v) for v in quarters.values()) == 6
    assert all(k[4] == "Q" for k in quarters)


# ------------------------------------------------------------- statistics


def test_sharpe_carries_its_standard_error():
    opens = [100.0, 100.0, 101.0, 101.0, 102.0, 102.0, 103.0, 103.0, 104.0, 104.0]
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", opens)}
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0 + k * 2 * MS_1H, weights={"hyperliquid:BTC": 1.0}
        )
        for k in range(4)
    ]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=2, notional_usd=1_000.0, tiers=TIERS, slippage_bps=0.0
    )
    stats = res.stats(periods_per_year=365 * 12)
    assert stats["status"] == "OK"
    assert stats["n_periods"] >= 2
    assert math.isfinite(stats["se_sharpe"])
    assert stats["se_bps"] > 0.0


def test_stats_refuses_to_report_a_ratio_on_one_observation():
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", [100.0] * 6)}
    reb = [claude_worker.bartest.Rebalance(ts=T0, weights={"hyperliquid:BTC": 1.0})]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=2, notional_usd=1_000.0, tiers=TIERS
    )
    assert res.stats(periods_per_year=1000.0)["status"] == "INSUFFICIENT"


# ------------------------------------------------------- the frozen contract


def _one_result():
    opens = [100.0, 101.0, 103.0, 102.0, 105.0, 104.0, 108.0, 107.0]
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", opens)}
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0 + k * 2 * MS_1H, weights={"hyperliquid:BTC": 1.0}
        )
        for k in range(3)
    ]
    return claude_worker.bartest.run(
        bars, reb, hold_bars=2, notional_usd=1_000.0, tiers=TIERS, slippage_bps=1.0
    )


def test_report_declares_the_accepted_audit_pnl_version():
    res = _one_result()
    obj = claude_worker.bartest.to_audit_pnl([(9, "kpnl-f1", res)], T0, T0 + 8 * MS_1H)
    assert obj["audit_pnl_version"] == claude_worker.pnl_report.AUDIT_PNL_VERSION
    assert obj["producer"] == "claude_worker.bartest"
    row = obj["strategies"][0]
    assert row["strategy_id"] == 9
    assert row["trades"] == res.n_trades
    assert float(row["net_usd"]) == pytest.approx(res.net_usd)
    # fees_usd carries the assumed slippage too -- it is a cost, not a gain
    assert float(row["fees_usd"]) == pytest.approx(res.fees_usd + res.slip_usd)


def test_report_round_trips_through_pnl_report():
    """The whole point of the frozen contract: the EXISTING day-report
    merge must fold a bartest report without knowing who wrote it."""
    res = _one_result()
    obj = claude_worker.bartest.to_audit_pnl([(9, "kpnl-f1", res)], T0, T0 + 8 * MS_1H)
    merged = claude_worker.pnl_report.merge_reports("2026-09-08", [("kpnl", obj)])
    assert merged["audit_pnl_version"] == 1
    rows = [r for r in merged["strategies"] if int(r["strategy_id"]) == 9]
    assert len(rows) == 1
    assert float(rows[0]["net_usd"]) == pytest.approx(res.net_usd)
    assert int(rows[0]["trades"]) == res.n_trades


def test_max_drawdown_is_peak_to_trough_of_the_period_equity():
    opens = [100.0, 100.0, 110.0, 110.0, 90.0, 90.0, 95.0, 95.0]
    bars = {"hyperliquid:BTC": make_bars("hyperliquid:BTC", opens)}
    reb = [
        claude_worker.bartest.Rebalance(
            ts=T0 + k * 2 * MS_1H, weights={"hyperliquid:BTC": 1.0}
        )
        for k in range(3)
    ]
    res = claude_worker.bartest.run(
        bars, reb, hold_bars=2, notional_usd=1_000.0, tiers=TIERS, slippage_bps=0.0
    )
    obj = claude_worker.bartest.to_audit_pnl([(9, "kpnl", res)], T0, T0 + 8 * MS_1H)
    assert float(obj["strategies"][0]["max_drawdown_usd"]) > 0.0


# ------------------------------------------------------------------ loading


def test_load_bars_drops_null_rows_and_respects_the_window():
    with tempfile.TemporaryDirectory() as d:
        db = pathlib.Path(d) / "c.db"
        conn = sqlite3.connect(db)
        conn.execute(
            "CREATE TABLE candles (venue INTEGER, descriptor TEXT, tf TEXT,"
            " open_ts INTEGER, o REAL, h REAL, l REAL, c REAL, v REAL,"
            " source TEXT, fetched_ts INTEGER, n INTEGER)"
        )
        rows = [
            (1, "x:y", "1h", T0 + 0 * MS_1H, 1.0, 1, 1, 1.0),
            (1, "x:y", "1h", T0 + 1 * MS_1H, None, 1, 1, 2.0),
            (1, "x:y", "1h", T0 + 2 * MS_1H, 3.0, 1, 1, 3.0),
            (1, "x:y", "1h", T0 + 9 * MS_1H, 9.0, 1, 1, 9.0),
        ]
        conn.executemany(
            "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c)"
            " VALUES (?,?,?,?,?,?,?,?)",
            rows,
        )
        conn.commit()
        bars = claude_worker.bartest.load_bars(
            conn, "x:y", "1h", T0, T0 + 5 * MS_1H
        )
    assert bars.open_ts == (T0, T0 + 2 * MS_1H)  # null dropped, out-of-window excluded
    assert bars.open_at(T0 + 1 * MS_1H) is None


def test_load_bars_rejects_an_unsupported_timeframe():
    with tempfile.TemporaryDirectory() as d:
        conn = sqlite3.connect(pathlib.Path(d) / "c.db")
        with pytest.raises(claude_worker.bartest.BartestError):
            claude_worker.bartest.load_bars(conn, "x:y", "3d", 0, 1)
