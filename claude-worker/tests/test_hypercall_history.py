# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""HC6: the Hypercall research-store pullers — cursor resume, payout
pages, the settlement-price table, the summary snapshot, the budget,
and the best-effort law. No live call: every body is canned.

Convention: full ``import x`` only.
"""

import json
import pathlib
import sqlite3

import claude_worker.candles
import claude_worker.features
import claude_worker.frames
import claude_worker.hypercall_history

HOST = "api.hypercall.xyz"
MAKER = "0xe55b5e5e38f73c30aa367d310d6247f3f9a5e86e"


def _trade(tid: int) -> dict:
    return {
        "trade_id": tid,
        "symbol": "BTC-20260927-83000-P",
        "price": "90.866",
        "size": "0.05502577",
        "maker_address": MAKER,
        "taker_address": "0xa4815c1a90c616404a98e9b0110f89b901eef9c9",
        "maker_fee": "0",
        "taker_fee": "0",
        "taker_side": "Buy",
        "timestamp": 1_790_361_650_000 + tid,
        "created_at": "2026-09-25T15:07:30Z",
    }


def _payout(rid: int, symbol: str, intrinsic: str, expiry_s: int = 1_790_366_400) -> dict:
    return {
        "id": rid,
        "wallet": MAKER,
        "symbol": symbol,
        "expiry_ts": expiry_s,
        "position_size": "-2.5",
        "settlement_price": intrinsic,
        "settlement_value": "0",
        "settlement_entry_price": "13.2425",
        "cost_basis": "-32.59",
        "net_pnl": "32.59",
        "ledger_applied": True,
        "is_seen": False,
        "created_at": 1_790_366_408_476,
    }


def _ok(rows: list[dict]) -> str:
    return json.dumps(
        {"success": True, "data": rows, "pagination": {"limit": 0, "offset": 0, "count": len(rows)}}
    )


class FakeVenue:
    """Serves canned pages; records every URL asked."""

    def __init__(
        self, trades: list[dict], payouts: list[dict], summary: dict[str, str | None]
    ) -> None:
        self.trades = trades
        self.payouts = payouts
        self.summary = summary
        self.urls: list[str] = []

    def get(self, url: str) -> str | None:
        self.urls.append(url)
        path, _, query = url.partition("?")
        q = dict(kv.split("=", 1) for kv in query.split("&") if "=" in kv)
        if path.endswith("/trades"):
            after, limit = int(q["after_trade_id"]), int(q["limit"])
            return _ok([t for t in self.trades if t["trade_id"] > after][:limit])
        if path.endswith("/settlement-payouts"):
            off, limit = int(q["offset"]), int(q["limit"])
            mine = [p for p in self.payouts if p["wallet"] == q["wallet"]]
            return _ok(mine[off : off + limit])
        if path.endswith("/options-summary"):
            return self.summary.get(q["currency"])
        return None

    def http(self) -> claude_worker.candles.Http:
        return claude_worker.candles.Http(get=self.get, post=lambda _u, _b: None, hosts={})


def _db(tmp_path: pathlib.Path) -> sqlite3.Connection:
    conn = claude_worker.candles.open_db(tmp_path / "candles.db")
    claude_worker.hypercall_history.ensure_schema(conn)
    return conn


def _budget(n: int = 1000) -> claude_worker.features.RestBudget:
    return claude_worker.features.RestBudget(n, 3_600_000_000_000)


def _ctx(
    conn: sqlite3.Connection, http: claude_worker.candles.Http, now_ms: int, calls: int = 1000
) -> claude_worker.hypercall_history.Ctx:
    return claude_worker.hypercall_history.Ctx(conn, http, HOST, _budget(calls), now_ms)


def test_trades_page_forward_from_the_stored_cursor_and_resume(
    tmp_path: pathlib.Path, monkeypatch
) -> None:
    monkeypatch.setattr(claude_worker.hypercall_history, "TRADES_PAGE", 3)
    venue = FakeVenue([_trade(t) for t in range(1, 8)], [], {})
    conn = _db(tmp_path)
    inserted, failed = claude_worker.hypercall_history.pull_trades(_ctx(conn, venue.http(), 1))
    assert (inserted, failed) == (7, 0)
    # Pages of 3: cursors 0, 3, 6 (the third page is short and ends it).
    assert [u.split("after_trade_id=")[1].split("&")[0] for u in venue.urls] == ["0", "3", "6"]
    row = conn.execute(
        "SELECT symbol, price, taker_side, maker FROM hc_trades WHERE trade_id = 5"
    ).fetchone()
    assert row == ("BTC-20260927-83000-P", 90.866, "Buy", MAKER)
    # A later cycle resumes from the stored cursor: only new rows land.
    venue.trades.append(_trade(8))
    venue.urls.clear()
    assert claude_worker.hypercall_history.pull_trades(_ctx(conn, venue.http(), 2)) == (
        1,
        0,
    )
    assert venue.urls[0].endswith("after_trade_id=7&limit=3")
    assert conn.execute("SELECT count(*) FROM hc_trades").fetchone()[0] == 8


def test_a_malformed_trade_row_is_skipped_and_a_dead_venue_is_counted(
    tmp_path: pathlib.Path,
) -> None:
    bad = _trade(2)
    bad["price"] = "n/a"
    venue = FakeVenue([_trade(1), bad, _trade(3)], [], {})
    conn = _db(tmp_path)
    assert claude_worker.hypercall_history.pull_trades(_ctx(conn, venue.http(), 1)) == (
        2,
        0,
    )
    dead = claude_worker.candles.Http(get=lambda _u: None, post=lambda _u, _b: None, hosts={})
    assert claude_worker.hypercall_history.pull_trades(_ctx(conn, dead, 1)) == (0, 1)
    assert claude_worker.hypercall_history.pull_trades(_ctx(conn, venue.http(), 1, 0)) == (
        0,
        0,
    ), "no budget, no call"


def test_payouts_page_until_nothing_is_new_and_pin_the_settlement(
    tmp_path: pathlib.Path, monkeypatch
) -> None:
    monkeypatch.setattr(claude_worker.hypercall_history, "PAYOUTS_PAGE", 2)
    payouts = [
        _payout(1, "SP500-20260925-7700-C", "42.2214"),  # S = 7742.2214
        _payout(2, "SP500-20260925-7800-P", "57.7786"),  # S = 7742.2214
        _payout(3, "SP500-20260925-7800-C", "0"),  # OTM: pins nothing
        _payout(4, "BOT-20260925-27.5-C", "0.871"),  # decimal strike: S = 28.371
        _payout(5, "AAPL-20260925-300-C", "40.9", 1_790_366_400),
        _payout(6, "AAPL-20260925-350-P", "9.3", 1_790_366_400),  # S = 340.7 ≠ 340.9
    ]
    venue = FakeVenue([], payouts, {})
    conn = _db(tmp_path)
    assert claude_worker.hypercall_history.pull_payouts(_ctx(conn, venue.http(), 1), [MAKER]) == (
        6,
        0,
    )
    table = {
        (u, e): (s, n, ok) for u, e, s, n, ok in claude_worker.hypercall_history.settle_prices(conn)
    }
    s, n, ok = table[("SP500", 1_790_366_400)]
    assert (round(s, 4), n, ok) == (7742.2214, 2, True)
    assert round(table[("BOT", 1_790_366_400)][0], 3) == 28.371
    assert table[("AAPL", 1_790_366_400)][2] is False, "two pins 0.2 apart are flagged"
    # The next cycle: the first page is already known, so it stops there.
    venue.urls.clear()
    assert claude_worker.hypercall_history.pull_payouts(_ctx(conn, venue.http(), 2), [MAKER]) == (
        0,
        0,
    )
    assert len(venue.urls) == 1


def test_the_summary_snapshot_keeps_marks_iv_and_the_provider_count(tmp_path: pathlib.Path) -> None:
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "result": [
                {
                    "instrument_name": "BTC-20261002-100000-C",
                    "mark_price": 1234.5,
                    "mark_iv": 0.52,
                    "underlying_price": 83912.7,
                    "open_interest": 12.25,
                    "bid_price": 1200.0,
                    "ask_price": 1260.0,
                    "rfq_provider_quotes": [{"wallet": MAKER}, {"wallet": "0x74ec"}],
                },
                {
                    "instrument_name": "BTC-20261002-110000-C",
                    "mark_price": None,
                    "rfq_provider_quotes": [],
                },
                {"mark_price": 1.0},
            ],
        }
    )
    venue = FakeVenue([], [], {"BTC": body, "ETH": "<html>"})
    conn = _db(tmp_path)
    assert claude_worker.hypercall_history.snapshot_summary(
        _ctx(conn, venue.http(), 7), ["BTC", "ETH"]
    ) == (2, 1)
    assert "include_rfq_provider_quotes=true" in venue.urls[0]
    row = conn.execute(
        "SELECT venue, mark_price, mark_iv, open_interest, bid, ask, providers FROM hc_summary"
        " WHERE instrument = 'BTC-20261002-100000-C'"
    ).fetchone()
    assert row == (claude_worker.frames.VENUE_HYPERCALL, 1234.5, 0.52, 12.25, 1200.0, 1260.0, 2)
    assert conn.execute(
        "SELECT mark_price, providers FROM hc_summary WHERE instrument = 'BTC-20261002-110000-C'"
    ).fetchone() == (None, 0)
    # The same cycle stamp twice is idempotent.
    assert claude_worker.hypercall_history.snapshot_summary(
        _ctx(conn, venue.http(), 7), ["BTC"]
    ) == (0, 0)


def test_a_cycle_runs_every_lane_under_one_budget(tmp_path: pathlib.Path) -> None:
    venue = FakeVenue(
        [_trade(1)], [_payout(1, "ETH-20260925-3250-C", "0")], {"ETH": json.dumps({"result": []})}
    )
    conn = _db(tmp_path)
    lines: list[str] = []
    stats = claude_worker.hypercall_history.run_cycle(
        _ctx(conn, venue.http(), 1, 100), ["ETH"], [MAKER], lines.append
    )
    assert stats == claude_worker.hypercall_history.CycleStats(
        trades=1, payouts=1, summary=0, failed=0, skipped=0
    )
    assert "payouts +1" in lines[-1]
    # No wallets: that lane is skipped, and says so.
    lines.clear()
    stats = claude_worker.hypercall_history.run_cycle(
        _ctx(conn, venue.http(), 2, 100), [], [], lines.append
    )
    assert stats.payouts == 0 and any("no wallets" in ln for ln in lines)
    # A budget of one call: only the trades lane gets it.
    stats = claude_worker.hypercall_history.run_cycle(
        _ctx(conn, venue.http(), 3, 1), ["ETH"], [MAKER], lines.append
    )
    assert stats.skipped == 2


def test_option_names_and_the_universe_section(tmp_path: pathlib.Path) -> None:
    assert claude_worker.hypercall_history.parse_option_name(
        "SP500-20260930-7742.5-P"
    ) == claude_worker.hypercall_history.OptionName("SP500", 7742.5, False)
    assert claude_worker.hypercall_history.parse_option_name(
        "BTC-20261002-100000-C"
    ) == claude_worker.hypercall_history.OptionName("BTC", 100000.0, True)
    for bad in (
        "BTC-PERPETUAL",
        "BTC-2026100-1-C",
        "BTC-20261002-x-C",
        "BTC-20261002-0-C",
        "-20261002-1-C",
    ):
        assert claude_worker.hypercall_history.parse_option_name(bad) is None, bad
    u = tmp_path / "universe.toml"
    u.write_text(
        '[hyperliquid]\ncoins = ["BTC", "#330"]\n'
        '[hypercall]\nunderlyings = ["SP500", "BTC"]\nexpiries = 3\n'
    )
    assert claude_worker.hypercall_history.read_underlyings(u) == ["SP500", "BTC"]
    u.write_text('[binance]\nspot = ["btcusdt"]\n')
    assert claude_worker.hypercall_history.read_underlyings(u) == []
    u.write_text("[hypercall\n")
    assert claude_worker.hypercall_history.read_underlyings(u) is None
    assert claude_worker.hypercall_history.read_underlyings(tmp_path / "missing.toml") is None


def test_main_prints_the_settle_table_from_the_store(tmp_path: pathlib.Path, capsys) -> None:
    conn = _db(tmp_path)
    venue = FakeVenue([], [_payout(1, "SP500-20260925-7700-C", "42.2214")], {})
    claude_worker.hypercall_history.pull_payouts(_ctx(conn, venue.http(), 1), [MAKER])
    conn.close()
    assert (
        claude_worker.hypercall_history.main(
            ["--db", str(tmp_path / "candles.db"), "--settle-prices"]
        )
        == 0
    )
    out = capsys.readouterr().out
    assert out.startswith("SP500\t2026-09-25T20:00:00+00:00\t7742.221400\t1\tok")
